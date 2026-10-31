//! Provenance and rollback of one rule's application
//! (architecture-concepts.md §1): the snapshot/diff machinery the executor
//! brackets every rule's query with, and the rollback that undoes what a
//! rule wrote. It knows nothing of plans, groups or resolution — only of
//! one rule, one episode and the graph the rule's Cypher wrote.
//!
//! The executor brackets each rule's query with two whole-store snapshots
//! inside the rule's own transaction, so it knows exactly which labels,
//! relationships and properties that rule wrote, whatever the query's text.
//! Labels are recorded as `(:RuleApplication {repository_id, rule,
//! episode})-[:LABELED {label}]->(n)` edges (a Cypher label cannot carry
//! properties); relationships the rule created are stamped
//! `concept`/`episode` and listed in the application's `provenance` map
//! beside the nodes it created and the properties it set. [`rollback`]
//! removes exactly those writes. Two whole-store reads per rule is the
//! price of provenance over arbitrary Cypher; §9's performance ceiling
//! question measures it. The snapshot is unscoped by repository: in a
//! shared multi-project store the per-rule cost carries the other projects'
//! nodes too. The per-repo-database topology (git-to-graph-plan.md §5)
//! makes this correct by construction; until then the ceiling question
//! carries the multiplier.
//!
//! Semantics the diff cannot undo: a rule that *deletes* nodes,
//! relationships, labels or properties (rather than adding them) cannot be
//! rolled back — the before-state is not kept, only the delta. Concepts are
//! enrichment (`MERGE`/`SET`); a destructive rule leaves its deletions in
//! place and is still counted as applied.
use super::model::Rule;
use crate::Result;
use crate::store_graph::{self, GraphStore, Params};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

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
fn remove_props(db: &mut cypherlite::Connection, rows: &[Value], on: &str) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    store_graph::write(
        db,
        &format!(
            "UNWIND $rows AS row MATCH {on} WHERE id(target) = row.id REMOVE target[row.prop]"
        ),
        store_graph::params(&json!({"rows": rows})),
    )?;
    Ok(rows.len())
}

/// What one rule's query wrote, as the diff between the snapshots around it.
#[derive(Default)]
pub(super) struct Delta {
    /// Nodes the rule created.
    pub(super) created_nodes: Vec<i64>,
    /// Relationships the rule created (deleted whole on rollback, with
    /// their properties).
    pub(super) created_rels: Vec<i64>,
    /// `(node, label)` pairs the rule added — on existing nodes and on the
    /// ones it created.
    pub(super) labels: Vec<(i64, String)>,
    /// Properties the rule set on nodes — added or changed, on existing
    /// nodes and on the ones it created.
    pub(super) node_props: Vec<(i64, String)>,
    /// Properties the rule set on relationships that existed before it.
    pub(super) rel_props: Vec<(i64, String)>,
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
pub(super) struct Snapshot {
    nodes: BTreeMap<i64, (BTreeSet<String>, Map<String, Value>)>,
    rels: BTreeMap<i64, Map<String, Value>>,
}

pub(super) fn snapshot(db: &mut cypherlite::Connection) -> Result<Snapshot> {
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
pub(super) fn diff(before: &Snapshot, after: &Snapshot) -> Delta {
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
pub(super) fn write_application(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch_rules::enrich;
    use crate::arch_rules::tests::{REPO, SHARED, count, rules_graph};

    #[test]
    fn rollback_removes_exactly_one_rules_writes() {
        let graph = rules_graph(SHARED, &["db_one", "db_two", "plain"]);
        let leasing = || Ok(());
        enrich(&graph, REPO, "ep-1", &leasing).unwrap();

        // The middle rule: a relationship between two symbols.
        let removed = rollback(&graph, REPO, "arch:Test/Connect", "ep-1").unwrap();
        assert_eq!(removed["relationships"], json!(1), "{removed:#}");
        assert_eq!(
            count(&graph, "MATCH ()-[r:PERSISTS_IN]->() RETURN count(r) AS n"),
            0,
            "the rule's relationship is gone"
        );
        assert_eq!(
            count(
                &graph,
                "MATCH (a:RuleApplication {rule: 'arch:Test/Connect'}) RETURN count(a) AS n"
            ),
            0
        );
        // Every other rule's writes are untouched.
        assert_eq!(
            count(&graph, "MATCH (s:Symbol:Persistence) RETURN count(s) AS n"),
            2
        );
        assert_eq!(
            count(
                &graph,
                "MATCH (s:Symbol) WHERE s.arch_confidence = 0.8 RETURN count(s) AS n"
            ),
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
        assert_eq!(
            count(&graph, "MATCH (s:Symbol:Persistence) RETURN count(s) AS n"),
            0
        );
        assert_eq!(count(&graph, "MATCH (s:Symbol) RETURN count(s) AS n"), 3);

        // The node-creating rule: the node goes with its rule.
        let removed = rollback(&graph, REPO, "arch:Test/Layer", "ep-1").unwrap();
        assert_eq!(removed["nodes"], json!(1), "{removed:#}");
        assert_eq!(count(&graph, "MATCH (l:Layer) RETURN count(l) AS n"), 0);

        // The property rule.
        let removed = rollback(&graph, REPO, "arch:Test/Confidence", "ep-1").unwrap();
        assert_eq!(removed["properties"], json!(3), "{removed:#}");
        assert_eq!(
            count(
                &graph,
                "MATCH (s) WHERE s.arch_confidence IS NOT NULL RETURN count(s) AS n"
            ),
            0
        );

        // A rule that never ran rolls back nothing.
        let none = rollback(&graph, REPO, "arch:Test/Missing", "ep-1").unwrap();
        assert_eq!(none["labels"], json!(0));
        assert_eq!(none["nodes"], json!(0));
    }
}
