//! The enrich executor (architecture-concepts.md §1, §2 and §5): applies a
//! project's concept rules to one recorded episode, topologically, one graph
//! transaction per rule.
//!
//! The executor does not parse rule bodies — a concept *is* its Cypher, run
//! as written with `$repo` (the repository id), `$episode` and `$rule` (the
//! rule id) as parameters. What it adds around them:
//!
//! - **Order and failure semantics**: the rules come back from the store as
//!   a [`RuleDocument`] and go through [`crate::arch_rules_dag::resolve`],
//!   so they run in dependency order with the groups' severity overrides
//!   applied. A rule whose query fails is rolled back whole (its own
//!   transaction), reported under `failed`, and its transitive dependents
//!   move to `skipped` ([`Resolution::skip_after`]) — never a half-applied
//!   state. Constraints are resolved and carried (their effective
//!   severities are part of the result) but never executed here; a later
//!   node evaluates them against the enriched graph.
//! - **Provenance**: the executor diffs the whole graph inside the rule's
//!   own transaction — a snapshot before the query, one after — so it knows
//!   exactly which labels, relationships and properties that rule wrote,
//!   whatever the query's text. Labels are recorded as
//!   `(:RuleApplication {repository_id, rule, episode})-[:LABELED
//!   {label}]->(n)` edges (a Cypher label cannot carry properties);
//!   relationships the rule created are stamped `concept`/`episode` and
//!   listed in the application's `provenance` map beside the nodes it
//!   created and the properties it set. [`rollback`] removes exactly those
//!   writes. Two whole-store reads per rule is the price of provenance over
//!   arbitrary Cypher; §9's performance ceiling question measures it.
//!   The snapshot is unscoped by repository: in a shared multi-project
//!   store the per-rule cost carries the other projects' nodes too. The
//!   per-repo-database topology (git-to-graph-plan.md §5) makes this
//!   correct by construction; until then the ceiling question carries the
//!   multiplier.
//! - **Idempotence**: a rule whose query is MERGE-based writes nothing the
//!   second time, so re-applying a rule set to the same episode — a
//!   replayed job, a retry — records nothing new. The application record
//!   itself MERGEs on `(repository_id, rule, episode)`.
//!
//! Semantics the diff cannot undo: a rule that *deletes* nodes,
//! relationships, labels or properties (rather than adding them) cannot be
//! rolled back — the before-state is not kept, only the delta. Concepts are
//! enrichment (`MERGE`/`SET`); a destructive rule leaves its deletions in
//! place and is still counted as applied.
//!
//! Which rules run: every group the project declared, in group-id order, a
//! rule applied once across the groups that select it; a project that
//! declared no group runs every rule (documents without groups — the common
//! case — clearly intend all of them).
use super::model::{Rule, RuleDocument, Severity};
use super::{Group, rule_set_hash};
use crate::Result;
use crate::store_graph::{self, GraphStore, Params};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// The group the executor resolves when the project declared none: every
/// rule, no overrides. Its id only appears in the result's plan reports.
const DEFAULT_GROUP: &str = "default";

/// Apply the project's concept rules to `episode` (architecture-concepts.md
/// §1.5: enrichment is the post-processing step of a recorded projection).
/// `still_leasing` is checked before every rule, so a job whose lease was
/// cancelled or taken over stops with that error. See the module doc for
/// order, failure and provenance semantics; the result is the job's:
///
/// ```json
/// {"kind": "enrich", "episode": "…", "rule_set": "…", "applied": 2,
///  "failed": 0, "skipped": 0, "plans": [{"group": "g",
///    "applied": [{"rule": "arch:A", "labels": 3, "relationships": 1,
///                 "properties": 3, "nodes": 1}],
///    "failed": [{"rule": "arch:B", "error": "…"}],
///    "skipped": ["arch:C"],
///    "severities": {"arch:A": "MINOR", "arch:K": "BLOCKER"}}]}
/// ```
///
/// `severities` covers every plan member, constraints included — the
/// constraint evaluator that follows reads them from here.
pub fn enrich(
    graph: &GraphStore,
    repository_id: &str,
    episode: &str,
    still_leasing: &dyn Fn() -> Result<()>,
) -> Result<Value> {
    let document = rule_document(graph, repository_id)?;
    let rule_set = rule_set_hash(graph, repository_id)?;
    let Some(document) = document else {
        return Ok(json!({
            "kind": "enrich", "episode": episode, "rule_set": rule_set,
            "plans": [], "applied": 0, "failed": 0, "skipped": 0,
        }));
    };
    let constraint_ids: BTreeSet<&str> = document
        .constraints
        .iter()
        .map(|rule| rule.id.as_str())
        .collect();
    let concepts: BTreeMap<&str, &Rule> = document
        .concepts
        .iter()
        .map(|rule| (rule.id.as_str(), rule))
        .collect();
    let mut applied = BTreeSet::new();
    // One plan per declared group, in group-id order; with no group, one
    // plan of every rule.
    let mut plans = Vec::new();
    if document.groups.is_empty() {
        let document = with_default_group(&document);
        plans.push(run_plan(
            graph,
            repository_id,
            episode,
            &document,
            DEFAULT_GROUP,
            &concepts,
            &constraint_ids,
            &mut applied,
            still_leasing,
        )?);
    } else {
        let mut groups: Vec<&str> = document.groups.iter().map(|g| g.id.as_str()).collect();
        groups.sort_unstable();
        for group in groups {
            plans.push(run_plan(
                graph,
                repository_id,
                episode,
                &document,
                group,
                &concepts,
                &constraint_ids,
                &mut applied,
                still_leasing,
            )?);
        }
    }
    let total = |key: &str| {
        plans
            .iter()
            .map(|plan| plan[key].as_array().map_or(0, Vec::len))
            .sum::<usize>()
    };
    let (applied_count, failed_count, skipped_count) =
        (total("applied"), total("failed"), total("skipped"));
    Ok(json!({
        "kind": "enrich", "episode": episode, "rule_set": rule_set,
        "plans": plans,
        "applied": applied_count, "failed": failed_count, "skipped": skipped_count,
    }))
}

/// Rollback of one rule's application to one episode
/// (architecture-concepts.md §1): the labels it added are removed (its
/// `:LABELED` edges name node and label), the relationships it created are
/// deleted, the properties it set are removed, and nodes it created are
/// deleted when the removals leave them with no label and no relationship.
/// The application record goes with them. A rule that was never applied to
/// the episode rolls back nothing.
pub fn rollback(
    graph: &GraphStore,
    repository_id: &str,
    rule: &str,
    episode: &str,
) -> Result<Value> {
    graph.transaction(|db| {
        let key = || {
            store_graph::params(&json!({"repo": repository_id, "rule": rule, "episode": episode}))
        };
        let application = store_graph::one(
            db,
            "MATCH (a:RuleApplication {repository_id: $repo, rule: $rule, episode: $episode}) \
             RETURN a.provenance AS provenance",
            key(),
        )?;
        let Some(application) = application else {
            return Ok(json!({"rule": rule, "episode": episode, "labels": 0, "relationships": 0,
                "properties": 0, "nodes": 0}));
        };
        let provenance: Value =
            serde_json::from_str(store_graph::s(&application, "provenance")).unwrap_or_default();
        let ids = |key: &str| -> Vec<i64> {
            provenance[key]
                .as_array()
                .map(|items| items.iter().filter_map(Value::as_i64).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        let props = |key: &str| -> Vec<(i64, String)> {
            provenance[key]
                .as_object()
                .map(|map| {
                    map.iter()
                        .filter_map(|(id, names)| {
                            Some((id.parse::<i64>().ok()?, names.clone()))
                        })
                        .flat_map(|(id, names)| {
                            names
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .map(move |name| (id, name.to_string()))
                                .collect::<Vec<_>>()
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        // The rule's labels, read from the application's own edges.
        let labeled: Vec<(i64, String)> = store_graph::rows(
            db,
            "MATCH (a:RuleApplication {repository_id: $repo, rule: $rule, episode: $episode})-[l:LABELED]->(n) \
             RETURN id(n) AS node, l.label AS label",
            key(),
        )?
        .into_iter()
        .filter_map(|row| {
            Some((
                row["node"].as_i64()?,
                store_graph::s(&row, "label").to_string(),
            ))
        })
        .collect();

        // The relationships the rule created go first, so the node sweep
        // below sees the graph without them.
        let rels = ids("rels");
        if !rels.is_empty() {
            store_graph::write(
                db,
                "MATCH ()-[r]->() WHERE id(r) IN $ids DELETE r",
                store_graph::params(&json!({"ids": rels})),
            )?;
        }
        if !labeled.is_empty() {
            let rows: Vec<Value> = labeled
                .iter()
                .map(|(node, label)| json!({"node": node, "label": label}))
                .collect();
            store_graph::write(
                db,
                "UNWIND $rows AS row MATCH (n) WHERE id(n) = row.node REMOVE n:$(row.label)",
                store_graph::params(&json!({"rows": rows})),
            )?;
        }
        let node_props: Vec<Value> = props("node_props")
            .into_iter()
            .map(|(id, prop)| json!({"id": id, "prop": prop}))
            .collect();
        let rel_props: Vec<Value> = props("rel_props")
            .into_iter()
            .map(|(id, prop)| json!({"id": id, "prop": prop}))
            .collect();
        let prop_count = remove_props(db, &node_props, "(target)")?
            + remove_props(db, &rel_props, "()-[target]->()")?;
        // The application's own edges, then the nodes it created that its
        // removals left empty, then the record itself.
        store_graph::write(
            db,
            "MATCH (a:RuleApplication {repository_id: $repo, rule: $rule, episode: $episode})-[l:LABELED]->() DELETE l",
            key(),
        )?;
        let created = ids("nodes");
        let mut emptied = Vec::new();
        if !created.is_empty() {
            emptied = store_graph::rows(
                db,
                "MATCH (n) WHERE id(n) IN $ids \
                 OPTIONAL MATCH (n)-[r]-() \
                 RETURN id(n) AS id, labels(n) AS labels, count(r) AS rels",
                store_graph::params(&json!({"ids": created})),
            )?
            .into_iter()
            .filter(|row| {
                row["labels"].as_array().is_none_or(Vec::is_empty)
                    && row["rels"].as_i64().unwrap_or(0) == 0
            })
            .filter_map(|row| row["id"].as_i64())
            .collect::<Vec<_>>();
            if !emptied.is_empty() {
                store_graph::write(
                    db,
                    "MATCH (n) WHERE id(n) IN $ids DELETE n",
                    store_graph::params(&json!({"ids": emptied})),
                )?;
            }
        }
        store_graph::write(
            db,
            "MATCH (a:RuleApplication {repository_id: $repo, rule: $rule, episode: $episode}) \
             DETACH DELETE a",
            key(),
        )?;
        Ok(json!({
            "rule": rule, "episode": episode,
            "labels": labeled.len(), "relationships": rels.len(),
            "properties": prop_count, "nodes": emptied.len(),
        }))
    })
}

/// Remove the properties `rows` names (`{id, prop}`) from the nodes or
/// relationships `on` matches (a variable named `target`).
fn remove_props(
    db: &mut cypherlite::Connection,
    rows: &[Value],
    on: &str,
) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    store_graph::write(
        db,
        &format!("UNWIND $rows AS row MATCH {on} WHERE id(target) = row.id REMOVE target[row.prop]"),
        store_graph::params(&json!({"rows": rows})),
    )?;
    Ok(rows.len())
}

/// Resolve one group and run its plan. Rules an earlier group already
/// applied in this run are not run again (their queries are MERGE-idempotent
/// either way; skipping keeps one failure semantics per run). `applied`
/// carries the applied ids across the groups of one run.
#[allow(clippy::too_many_arguments)]
fn run_plan(
    graph: &GraphStore,
    repository_id: &str,
    episode: &str,
    document: &RuleDocument,
    group: &str,
    concepts: &BTreeMap<&str, &Rule>,
    constraint_ids: &BTreeSet<&str>,
    applied: &mut BTreeSet<String>,
    still_leasing: &dyn Fn() -> Result<()>,
) -> Result<Value> {
    let resolution = match crate::arch_rules_dag::resolve(document, group) {
        Ok(resolution) => resolution,
        Err(error) => {
            return Ok(json!({"group": group, "error": error.to_string(), "applied": [],
                "failed": [], "skipped": [], "severities": {}}));
        }
    };
    let mut resolution = resolution;
    // The plan as resolved; failures below remove members from it again.
    let mut queue: VecDeque<String> = resolution.order.iter().cloned().collect();
    let mut applied_rules = Vec::new();
    let mut failed = Vec::new();
    while let Some(id) = queue.pop_front() {
        // Dropped by an earlier failure's skip_after, a constraint (the
        // later node evaluates those), or an earlier group of this run.
        if !resolution.order.contains(&id)
            || constraint_ids.contains(id.as_str())
            || applied.contains(&id)
        {
            continue;
        }
        let Some(rule) = concepts.get(id.as_str()) else {
            continue;
        };
        still_leasing()?;
        match apply_rule(graph, repository_id, episode, rule) {
            Ok(report) => {
                applied.insert(id.clone());
                applied_rules.push(report);
            }
            Err(error) => {
                failed.push(json!({"rule": id, "error": error.message}));
                resolution = resolution.skip_after(&[id.as_str()]);
            }
        }
    }
    let severities: Map<String, Value> = resolution
        .severities
        .iter()
        .map(|(id, severity)| (id.clone(), json!(severity.as_str())))
        .collect();
    Ok(json!({
        "group": group,
        "applied": applied_rules,
        "failed": failed,
        "skipped": resolution.skipped,
        "severities": Value::Object(severities),
    }))
}

/// `document` with the every-rule group of a project that declared none
/// (see [`DEFAULT_GROUP`]).
fn with_default_group(document: &RuleDocument) -> RuleDocument {
    let mut document = document.clone();
    document.groups.push(Group {
        id: DEFAULT_GROUP.to_string(),
        description: "every rule of the project".to_string(),
        include: vec!["*".to_string()],
        overrides: Vec::new(),
        line: 0,
        rule_sha: String::new(),
    });
    document
}

/// The rule set of `repository_id` read back from its store nodes as a
/// [`RuleDocument`]: every `:ConceptRule`, `:ConstraintRule` and
/// `:RuleGroup` in `(source, line)` order — the order the rule documents
/// declared them in, which the resolver's tie-breaking keeps. `None` when
/// the project holds no rule nodes.
pub(super) fn rule_document(graph: &GraphStore, repository_id: &str) -> Result<Option<RuleDocument>> {
    let rules = |label: &str| -> Result<Vec<(String, usize, Rule)>> {
        let rows = graph.read(|db| {
            store_graph::rows(
                db,
                &format!(
                    "MATCH (r:{label} {{repository_id: $repo}}) \
                     RETURN r.id AS id, r.description AS description, r.requires AS requires, \
                            r.severity AS severity, r.cypher AS cypher, r.rule_sha AS rule_sha, \
                            r.source AS source, r.line AS line"
                ),
                store_graph::params(&json!({"repo": repository_id})),
            )
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let severity = Severity::parse(store_graph::s(&row, "severity"))?;
                let requires: Vec<String> = serde_json::from_str(store_graph::s(&row, "requires"))
                    .unwrap_or_default();
                let line = row["line"].as_i64().unwrap_or_default() as usize;
                Some((
                    store_graph::s(&row, "source").to_string(),
                    line,
                    Rule {
                        id: store_graph::s(&row, "id").to_string(),
                        description: store_graph::s(&row, "description").to_string(),
                        requires,
                        severity,
                        cypher: store_graph::s(&row, "cypher").to_string(),
                        line,
                        rule_sha: store_graph::s(&row, "rule_sha").to_string(),
                    },
                ))
            })
            .collect())
    };
    let mut concepts = rules("ConceptRule")?;
    let mut constraints = rules("ConstraintRule")?;
    let mut groups: Vec<Group> = graph
        .read(|db| {
            store_graph::rows(
                db,
                "MATCH (r:RuleGroup {repository_id: $repo}) \
                 RETURN r.id AS id, r.description AS description, r.include AS include, \
                        r.overrides AS overrides, r.rule_sha AS rule_sha, r.source AS source, \
                        r.line AS line",
                store_graph::params(&json!({"repo": repository_id})),
            )
        })?
        .into_iter()
        .filter_map(|row| {
            let include: Vec<String> =
                serde_json::from_str(store_graph::s(&row, "include")).unwrap_or_default();
            let stored: Vec<String> =
                serde_json::from_str(store_graph::s(&row, "overrides")).unwrap_or_default();
            let mut overrides = Vec::new();
            for item in stored {
                let (pattern, severity) = item.split_once('=')?;
                overrides.push((pattern.to_string(), Severity::parse(severity.trim())?));
            }
            if include.is_empty() {
                return None;
            }
            Some(Group {
                id: store_graph::s(&row, "id").to_string(),
                description: store_graph::s(&row, "description").to_string(),
                include,
                overrides,
                line: row["line"].as_i64().unwrap_or_default() as usize,
                rule_sha: store_graph::s(&row, "rule_sha").to_string(),
            })
        })
        .collect();
    if concepts.is_empty() && constraints.is_empty() && groups.is_empty() {
        return Ok(None);
    }
    let by_place =
        |a: &(String, usize, Rule), b: &(String, usize, Rule)| a.0.cmp(&b.0).then(a.1.cmp(&b.1));
    concepts.sort_by(by_place);
    constraints.sort_by(by_place);
    let concepts = concepts.into_iter().map(|(_, _, rule)| rule).collect();
    let constraints = constraints.into_iter().map(|(_, _, rule)| rule).collect();
    groups.sort_by(|a, b| a.id.cmp(&b.id).then(a.line.cmp(&b.line)));
    Ok(Some(RuleDocument {
        concepts,
        constraints,
        groups,
    }))
}

/// One rule's application to one episode: the rule's query runs in its own
/// graph transaction, between two whole-store snapshots, and the delta
/// between them becomes the application's provenance. A failure rolls the
/// transaction back — the rule leaves nothing behind — and surfaces as the
/// caller's `failed` entry.
fn apply_rule(
    graph: &GraphStore,
    repository_id: &str,
    episode: &str,
    rule: &Rule,
) -> Result<Value> {
    graph.transaction(|db| {
        let before = snapshot(db)?;
        store_graph::write(
            db,
            &rule.cypher,
            store_graph::params(&json!({
                "repo": repository_id, "episode": episode, "rule": rule.id,
            })),
        )?;
        let delta = diff(&before, &snapshot(db)?);
        write_application(db, repository_id, episode, rule, &delta)?;
        Ok(json!({
            "rule": rule.id,
            "labels": delta.labels.len(),
            "relationships": delta.created_rels.len(),
            "properties": delta.node_props.len() + delta.rel_props.len(),
            "nodes": delta.created_nodes.len(),
        }))
    })
}

/// What one rule's query wrote, as the diff between the snapshots around it.
#[derive(Default)]
struct Delta {
    /// Nodes the rule created.
    created_nodes: Vec<i64>,
    /// Relationships the rule created (deleted whole on rollback, with
    /// their properties).
    created_rels: Vec<i64>,
    /// `(node, label)` pairs the rule added — on existing nodes and on the
    /// ones it created.
    labels: Vec<(i64, String)>,
    /// Properties the rule set on nodes — added or changed, on existing
    /// nodes and on the ones it created.
    node_props: Vec<(i64, String)>,
    /// Properties the rule set on relationships that existed before it.
    rel_props: Vec<(i64, String)>,
}

impl Delta {
    fn is_empty(&self) -> bool {
        self.created_nodes.is_empty()
            && self.created_rels.is_empty()
            && self.labels.is_empty()
            && self.node_props.is_empty()
            && self.rel_props.is_empty()
    }
}

/// The store's nodes and relationships as they stand: the diff substrate.
/// Two of these bracket one rule's query (see the module doc for the cost).
struct Snapshot {
    nodes: BTreeMap<i64, (BTreeSet<String>, Map<String, Value>)>,
    rels: BTreeMap<i64, Map<String, Value>>,
}

fn snapshot(db: &mut cypherlite::Connection) -> Result<Snapshot> {
    let nodes = store_graph::rows(
        db,
        "MATCH (n) RETURN id(n) AS id, labels(n) AS labels, properties(n) AS props",
        Params::new(),
    )?;
    let rels = store_graph::rows(
        db,
        "MATCH ()-[r]->() RETURN id(r) AS id, properties(r) AS props",
        Params::new(),
    )?;
    let nodes = nodes
        .into_iter()
        .filter_map(|row| {
            let id = row["id"].as_i64()?;
            let labels = row["labels"]
                .as_array()
                .map(|labels| {
                    labels
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<BTreeSet<_>>()
                })
                .unwrap_or_default();
            let props = row["props"].as_object().cloned().unwrap_or_default();
            Some((id, (labels, props)))
        })
        .collect();
    let rels = rels
        .into_iter()
        .filter_map(|row| {
            let id = row["id"].as_i64()?;
            let props = row["props"].as_object().cloned().unwrap_or_default();
            Some((id, props))
        })
        .collect();
    Ok(Snapshot { nodes, rels })
}

/// The delta from `before` to `after`: the writes a rule made. Additive
/// writes only — deletions cannot be rolled back (see the module doc).
fn diff(before: &Snapshot, after: &Snapshot) -> Delta {
    let mut delta = Delta::default();
    for (id, (labels, props)) in &after.nodes {
        match before.nodes.get(id) {
            None => {
                delta.created_nodes.push(*id);
                delta
                    .labels
                    .extend(labels.iter().map(|label| (*id, label.clone())));
                delta
                    .node_props
                    .extend(props.keys().map(|prop| (*id, prop.clone())));
            }
            Some((old_labels, old_props)) => {
                delta.labels.extend(
                    labels
                        .iter()
                        .filter(|label| !old_labels.contains(*label))
                        .map(|label| (*id, label.clone())),
                );
                delta.node_props.extend(
                    props
                        .iter()
                        .filter(|(prop, value)| old_props.get(*prop) != Some(value))
                        .map(|(prop, _)| (*id, prop.clone())),
                );
            }
        }
    }
    for (id, props) in &after.rels {
        match before.rels.get(id) {
            None => delta.created_rels.push(*id),
            Some(old_props) => {
                delta.rel_props.extend(
                    props
                        .iter()
                        .filter(|(prop, value)| old_props.get(*prop) != Some(value))
                        .map(|(prop, _)| (*id, prop.clone())),
                );
            }
        }
    }
    delta
}

/// Record the rule's application: the `:RuleApplication` node (MERGEd, so a
/// replay of the same rule and episode writes nothing), its `:LABELED`
/// edges for the labels, the `concept`/`episode` stamps on the relationships
/// the rule created, and the provenance map the rollback reads.
fn write_application(
    db: &mut cypherlite::Connection,
    repository_id: &str,
    episode: &str,
    rule: &Rule,
    delta: &Delta,
) -> Result<()> {
    let key = || {
        store_graph::params(&json!({"repo": repository_id, "rule": rule.id, "episode": episode}))
    };
    store_graph::write(
        db,
        "MERGE (a:RuleApplication {repository_id: $repo, rule: $rule, episode: $episode})",
        key(),
    )?;
    if delta.is_empty() {
        return Ok(());
    }
    let grouped = |items: &[(i64, String)]| -> Value {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (id, name) in items {
            map.entry(id.to_string()).or_default().push(name.clone());
        }
        serde_json::to_value(map).unwrap_or_default()
    };
    let provenance = json!({
        "nodes": delta.created_nodes,
        "rels": delta.created_rels,
        "node_props": grouped(&delta.node_props),
        "rel_props": grouped(&delta.rel_props),
    })
    .to_string();
    store_graph::write(
        db,
        "MATCH (a:RuleApplication {repository_id: $repo, rule: $rule, episode: $episode}) \
         SET a.provenance = $provenance, a.applied_at = $now",
        store_graph::params(&json!({
            "repo": repository_id, "rule": rule.id, "episode": episode,
            "provenance": provenance, "now": crate::now(),
        })),
    )?;
    if !delta.labels.is_empty() {
        let rows: Vec<Value> = delta
            .labels
            .iter()
            .map(|(node, label)| json!({"node": node, "label": label}))
            .collect();
        store_graph::write(
            db,
            "UNWIND $rows AS row \
             MATCH (a:RuleApplication {repository_id: $repo, rule: $rule, episode: $episode}), (n) \
             WHERE id(n) = row.node \
             MERGE (a)-[:LABELED {label: row.label}]->(n)",
            store_graph::params(&json!({
                "repo": repository_id, "rule": rule.id, "episode": episode, "rows": rows,
            })),
        )?;
    }
    if !delta.created_rels.is_empty() {
        store_graph::write(
            db,
            "MATCH ()-[r]->() WHERE id(r) IN $ids SET r.concept = $rule, r.episode = $episode",
            store_graph::params(&json!({
                "ids": delta.created_rels, "rule": rule.id, "episode": episode,
            })),
        )?;
    }
    Ok(())
}

/// Whether the project holds any rule nodes (a concept or a constraint):
/// the signal that a completed projection queues an enrich job.
pub fn has_rules(graph: &GraphStore, repository_id: &str) -> Result<bool> {
    graph.read(|db| {
        for label in ["ConceptRule", "ConstraintRule"] {
            let found = store_graph::one(
                db,
                &format!("MATCH (r:{label} {{repository_id: $repo}}) RETURN count(r) AS n"),
                store_graph::params(&json!({"repo": repository_id})),
            )?
            .and_then(|row| row["n"].as_i64())
            .unwrap_or(0);
            if found > 0 {
                return Ok(true);
            }
        }
        Ok(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch_rules::{parse, store as store_rules};

    const REPO: &str = "repo";

    /// An in-memory store holding `rules` (a Markdown rule document) and the
    /// symbols the rules work on.
    fn fixture(rules: &str, symbols: &[&str]) -> GraphStore {
        let graph = GraphStore::open_in_memory().unwrap();
        let document = parse(rules).unwrap();
        store_rules(&graph, REPO, "rules.md", &document).unwrap();
        add_symbols(&graph, symbols);
        graph
    }

    fn add_symbols(graph: &GraphStore, ids: &[&str]) {
        graph
            .transaction(|db| {
                for id in ids {
                    store_graph::write(
                        db,
                        "CREATE (s:Symbol {repository_id: $repo, stable_id: $id, \
                         kind: 'function', qualified_name: $id, relpath: 'src/lib.rs'})",
                        store_graph::params(&json!({"repo": REPO, "id": id})),
                    )?;
                }
                Ok(())
            })
            .unwrap();
    }

    fn enrich(graph: &GraphStore, episode: &str) -> Value {
        let leasing = || Ok(());
        super::enrich(graph, REPO, episode, &leasing).unwrap()
    }

    fn count(graph: &GraphStore, cypher: &str) -> i64 {
        graph
            .read(|db| {
                store_graph::rows(db, cypher, store_graph::params(&json!({"repo": REPO})))
            })
            .unwrap()[0]["n"]
            .as_i64()
            .unwrap()
    }

    /// The store as one comparable value: every node with its labels and
    /// properties, every relationship with type, endpoints and properties.
    fn state(graph: &GraphStore) -> Value {
        graph
            .read(|db| {
                let nodes = store_graph::rows(
                    db,
                    "MATCH (n) RETURN id(n) AS id, labels(n) AS labels, properties(n) AS props",
                    Params::new(),
                )?;
                let rels = store_graph::rows(
                    db,
                    "MATCH ()-[r]->() RETURN id(r) AS id, type(r) AS type, properties(r) AS props",
                    Params::new(),
                )?;
                Ok(json!({"nodes": nodes, "rels": rels}))
            })
            .unwrap()
    }

    /// The rules the provenance tests share: a label, a relationship, a
    /// property and a created node, one rule each, plus a constraint.
    const SHARED: &str = "````concept\nid: arch:Test/Label\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) WHERE s.stable_id STARTS WITH 'db'\nSET s:Persistence\n```\n````\n\
````concept\nid: arch:Test/Connect\nrequires: [arch:Test/Label]\n```cypher\nMATCH (a:Symbol {repository_id: $repo, stable_id: 'db_one'}), (b:Symbol {repository_id: $repo, stable_id: 'db_two'})\nMERGE (a)-[:PERSISTS_IN]->(b)\n```\n````\n\
````concept\nid: arch:Test/Confidence\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) SET s.arch_confidence = 0.8\n```\n````\n\
````concept\nid: arch:Test/Layer\n```cypher\nMERGE (l:Layer {repository_id: $repo, name: 'Persistence'})\n```\n````\n\
````constraint\nid: arch:Test/NoCycles\nseverity: BLOCKER\nrequires: [arch:Test/Connect]\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) RETURN s\n```\n````\n";

    #[test]
    fn applies_concepts_in_resolution_order_and_carries_constraints() {
        let graph = fixture(SHARED, &["db_one", "db_two", "plain"]);
        let report = enrich(&graph, "ep-1");

        // Every concept ran; the constraint did not (a later node evaluates
        // it) but its severity is carried through.
        assert_eq!(report["applied"], json!(4), "{report:#}");
        assert_eq!(report["failed"], json!(0), "{report:#}");
        assert_eq!(report["skipped"], json!(0), "{report:#}");
        let plan = &report["plans"][0];
        assert_eq!(plan["group"], json!("default"));
        assert_eq!(
            plan["severities"]["arch:Test/NoCycles"],
            json!("BLOCKER"),
            "{report:#}"
        );
        let applied: Vec<&str> = plan["applied"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["rule"].as_str())
            .collect();
        // Resolution order: Label before the Connect that requires it.
        assert_eq!(
            applied,
            vec![
                "arch:Test/Label",
                "arch:Test/Connect",
                "arch:Test/Confidence",
                "arch:Test/Layer",
            ],
            "{report:#}"
        );

        // The writes, each provenance-tracked under its rule.
        assert_eq!(
            count(&graph, "MATCH (s:Symbol:Persistence) RETURN count(s) AS n"),
            2
        );
        assert_eq!(
            count(
                &graph,
                "MATCH (:Symbol {stable_id: 'db_one'})-[r:PERSISTS_IN]->(:Symbol {stable_id: 'db_two'}) \
                 WHERE r.concept = 'arch:Test/Connect' AND r.episode = 'ep-1' RETURN count(r) AS n"
            ),
            1
        );
        assert_eq!(
            count(&graph, "MATCH (s:Symbol) WHERE s.arch_confidence = 0.8 RETURN count(s) AS n"),
            3
        );
        assert_eq!(
            count(&graph, "MATCH (l:Layer {name: 'Persistence'}) RETURN count(l) AS n"),
            1
        );
        // One application record per concept and episode.
        assert_eq!(
            count(&graph, "MATCH (a:RuleApplication {episode: 'ep-1'}) RETURN count(a) AS n"),
            4
        );
        // The labels are recorded as LABELED edges of the rule that set them.
        assert_eq!(
            count(
                &graph,
                "MATCH (:RuleApplication {rule: 'arch:Test/Label', episode: 'ep-1'})-[:LABELED {label: 'Persistence'}]->(:Symbol) RETURN count(*) AS n"
            ),
            2
        );
    }

    /// The failure probe's rules: [`SHARED`]'s Label, a Connect whose query
    /// writes and then fails, a dependent of it, an independent concept and
    /// a constraint on the dependent.
    const PROBE_RULES: &str = "````concept\nid: arch:Test/Label\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) WHERE s.stable_id STARTS WITH 'db'\nSET s:Persistence\n```\n````\n\
````concept\nid: arch:Test/Connect\nrequires: [arch:Test/Label]\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) SET s.b_mark = 'x'\nCREATE (o:Object {repository_id: $repo, sha256: 'taken'})\n```\n````\n\
````concept\nid: arch:Test/Dependent\nrequires: [arch:Test/Connect]\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) SET s.c_mark = 'x'\n```\n````\n\
````concept\nid: arch:Test/Free\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) SET s.d_mark = 'x'\n```\n````\n\
````constraint\nid: arch:Test/NoCycles\nseverity: BLOCKER\nrequires: [arch:Test/Dependent]\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) RETURN s\n```\n````\n";

    #[test]
    fn a_failed_rule_rolls_back_whole_and_skips_its_dependents() {
        // Connect fails after a write of its own: the SET lands inside the
        // statement, the CREATE violates the Object key, and the whole
        // transaction — SET included — must roll back.
        let graph = graph_with_probe();
        let report = enrich(&graph, "ep-1");

        assert_eq!(report["applied"], json!(2), "{report:#}");
        let failed = report["plans"][0]["failed"].as_array().unwrap().clone();
        assert_eq!(failed.len(), 1, "{report:#}");
        assert_eq!(failed[0]["rule"], json!("arch:Test/Connect"));
        assert!(!failed[0]["error"].as_str().unwrap().is_empty());
        // The concept that requires the failed one is skipped; the one that
        // requires nothing runs on.
        let skipped = report["plans"][0]["skipped"].as_array().unwrap().clone();
        assert_eq!(
            skipped.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["arch:Test/Dependent", "arch:Test/NoCycles"],
            "{report:#}"
        );
        // Nothing of the failed rule is left behind.
        assert_eq!(
            count(&graph, "MATCH (s) WHERE s.b_mark IS NOT NULL RETURN count(s) AS n"),
            0
        );
        assert_eq!(
            count(&graph, "MATCH (a:RuleApplication {rule: 'arch:Test/Connect'}) RETURN count(a) AS n"),
            0
        );
        // Its dependency and the independent rule both applied.
        assert_eq!(
            count(&graph, "MATCH (s:Symbol:Persistence) RETURN count(s) AS n"),
            2
        );
        assert_eq!(
            count(&graph, "MATCH (s:Symbol) WHERE s.d_mark = 'x' RETURN count(s) AS n"),
            3
        );
    }

    /// The failure probe's store: [`PROBE_RULES`] over an Object node whose
    /// key the failing CREATE duplicates.
    fn graph_with_probe() -> GraphStore {
        let graph = fixture(PROBE_RULES, &["db_one", "db_two", "plain"]);
        graph
            .transaction(|db| {
                store_graph::write(
                    db,
                    "CREATE (o:Object {repository_id: $repo, sha256: 'taken'})",
                    store_graph::params(&json!({"repo": REPO})),
                )?;
                Ok(())
            })
            .unwrap();
        graph
    }

    #[test]
    fn reapplying_the_same_rule_set_to_the_same_episode_changes_nothing() {
        let graph = fixture(SHARED, &["db_one", "db_two", "plain"]);
        enrich(&graph, "ep-1");
        let before = state(&graph);
        let again = enrich(&graph, "ep-1");
        assert_eq!(state(&graph), before, "the second apply must write nothing");
        // The second run reports the same rules with zero writes.
        let applied = again["plans"][0]["applied"].as_array().unwrap().clone();
        assert_eq!(applied.len(), 4, "{again:#}");
        for rule in applied {
            assert_eq!(rule["labels"], json!(0), "{again:#}");
            assert_eq!(rule["relationships"], json!(0), "{again:#}");
            assert_eq!(rule["properties"], json!(0), "{again:#}");
            assert_eq!(rule["nodes"], json!(0), "{again:#}");
        }
    }

    #[test]
    fn rollback_removes_exactly_one_rules_writes() {
        let graph = fixture(SHARED, &["db_one", "db_two", "plain"]);
        enrich(&graph, "ep-1");

        // The middle rule: a relationship between two symbols.
        let removed = rollback(&graph, REPO, "arch:Test/Connect", "ep-1").unwrap();
        assert_eq!(removed["relationships"], json!(1), "{removed:#}");
        assert_eq!(
            count(&graph, "MATCH ()-[r:PERSISTS_IN]->() RETURN count(r) AS n"),
            0,
            "the rule's relationship is gone"
        );
        assert_eq!(
            count(&graph, "MATCH (a:RuleApplication {rule: 'arch:Test/Connect'}) RETURN count(a) AS n"),
            0
        );
        // Every other rule's writes are untouched.
        assert_eq!(count(&graph, "MATCH (s:Symbol:Persistence) RETURN count(s) AS n"), 2);
        assert_eq!(
            count(&graph, "MATCH (s:Symbol) WHERE s.arch_confidence = 0.8 RETURN count(s) AS n"),
            3
        );
        assert_eq!(count(&graph, "MATCH (l:Layer) RETURN count(l) AS n"), 1);
        assert_eq!(
            count(&graph, "MATCH (a:RuleApplication) RETURN count(a) AS n"),
            3
        );

        // The label rule: labels off the symbols it labelled (which it did
        // not create), the symbols themselves kept.
        let removed = rollback(&graph, REPO, "arch:Test/Label", "ep-1").unwrap();
        assert_eq!(removed["labels"], json!(2), "{removed:#}");
        assert_eq!(removed["nodes"], json!(0), "{removed:#}");
        assert_eq!(count(&graph, "MATCH (s:Symbol:Persistence) RETURN count(s) AS n"), 0);
        assert_eq!(count(&graph, "MATCH (s:Symbol) RETURN count(s) AS n"), 3);

        // The node-creating rule: the node goes with its rule.
        let removed = rollback(&graph, REPO, "arch:Test/Layer", "ep-1").unwrap();
        assert_eq!(removed["nodes"], json!(1), "{removed:#}");
        assert_eq!(count(&graph, "MATCH (l:Layer) RETURN count(l) AS n"), 0);

        // The property rule.
        let removed = rollback(&graph, REPO, "arch:Test/Confidence", "ep-1").unwrap();
        assert_eq!(removed["properties"], json!(3), "{removed:#}");
        assert_eq!(
            count(&graph, "MATCH (s) WHERE s.arch_confidence IS NOT NULL RETURN count(s) AS n"),
            0
        );

        // A rule that never ran rolls back nothing.
        let none = rollback(&graph, REPO, "arch:Test/Missing", "ep-1").unwrap();
        assert_eq!(none["labels"], json!(0));
        assert_eq!(none["nodes"], json!(0));
    }

    #[test]
    fn each_episode_enriches_on_its_own_and_keeps_its_applications() {
        let graph = fixture(SHARED, &["db_one", "db_two"]);
        enrich(&graph, "ep-1");
        // The next episode's graph: one more symbol for the rules to find.
        add_symbols(&graph, &["db_late"]);
        let report = enrich(&graph, "ep-2");

        // The new symbol was labelled by the ep-2 run — recomputed, not
        // copied: its LABELED edge belongs to ep-2's application.
        assert_eq!(
            count(
                &graph,
                "MATCH (:RuleApplication {rule: 'arch:Test/Label', episode: 'ep-2'})-[:LABELED {label: 'Persistence'}]->(:Symbol {stable_id: 'db_late'}) RETURN count(*) AS n"
            ),
            1,
            "{report:#}"
        );
        // Both episodes keep their own application records.
        assert_eq!(
            count(&graph, "MATCH (a:RuleApplication {episode: 'ep-1'}) RETURN count(a) AS n"),
            4
        );
        assert_eq!(
            count(&graph, "MATCH (a:RuleApplication {episode: 'ep-2'}) RETURN count(a) AS n"),
            4
        );
        // The episodes' applications are distinguishable: ep-1's label rule
        // recorded the two symbols it labelled then, and only those.
        assert_eq!(
            count(
                &graph,
                "MATCH (:RuleApplication {rule: 'arch:Test/Label', episode: 'ep-1'})-[:LABELED]->(s:Symbol) RETURN count(s) AS n"
            ),
            2
        );
        assert_eq!(
            count(
                &graph,
                "MATCH (:RuleApplication {rule: 'arch:Test/Label', episode: 'ep-2'})-[:LABELED]->(s:Symbol) RETURN count(s) AS n"
            ),
            1
        );
    }

    #[test]
    fn a_declared_group_selects_rules_and_overrides_their_severities() {
        let rules = "````concept\nid: arch:A\nseverity: MINOR\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) SET s:Grouped\n```\n````\n\
````concept\nid: arch:B\nseverity: MAJOR\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) SET s.b_prop = 1\n```\n````\n\
````group\nid: arch:picked\ninclude: [arch:A]\noverrides: [arch:A=CRITICAL]\n````\n";
        let graph = fixture(rules, &["one"]);
        let report = enrich(&graph, "ep-1");
        assert_eq!(report["applied"], json!(1), "{report:#}");
        let plan = &report["plans"][0];
        assert_eq!(plan["group"], json!("arch:picked"));
        assert_eq!(plan["severities"]["arch:A"], json!("CRITICAL"), "{report:#}");
        // The rule the group does not include did not run.
        assert_eq!(count(&graph, "MATCH (s) WHERE s.b_prop IS NOT NULL RETURN count(s) AS n"), 0);
        assert_eq!(count(&graph, "MATCH (s:Symbol:Grouped) RETURN count(s) AS n"), 1);
    }

    #[test]
    fn has_rules_reads_the_stored_rule_nodes() {
        let with = fixture(SHARED, &[]);
        assert!(has_rules(&with, REPO).unwrap());
        assert!(!has_rules(&with, "other").unwrap());
        let without = GraphStore::open_in_memory().unwrap();
        assert!(!has_rules(&without, REPO).unwrap());
    }
}
