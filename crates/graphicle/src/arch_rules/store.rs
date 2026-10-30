//! Storage of a parsed rule document: `(:ConceptRule)`, `(:ConstraintRule)`
//! and `(:RuleGroup)` nodes scoped to one project, keyed by
//! `(repository_id, id)`. Each node carries a `rule_sha`: a digest of the
//! rule's content, so re-parsing identical text yields the same value. The
//! store is the same whichever frontend read the document.
use super::model::{Rule, RuleDocument};
use crate::Result;
use crate::store_graph::{self, GraphStore, Params};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Write the document's rules as nodes scoped to `repository_id`, merged on
/// `(repository_id, id)` so a re-parse updates in place. `source` is the
/// document's path, kept as evidence of where a rule was declared.
pub fn store(
    graph: &GraphStore,
    repository_id: &str,
    source: &str,
    document: &RuleDocument,
) -> Result<()> {
    graph.transaction(|db| write_rules(db, repository_id, source, document))
}

/// The document's rules written under `repository_id` (see [`store`]).
fn write_rules(
    db: &mut cypherlite::Connection,
    repository_id: &str,
    source: &str,
    document: &RuleDocument,
) -> Result<()> {
    for rule in &document.concepts {
        merge_rule(db, "ConceptRule", repository_id, source, rule)?;
    }
    for rule in &document.constraints {
        merge_rule(db, "ConstraintRule", repository_id, source, rule)?;
    }
    for group in &document.groups {
        let overrides: Vec<String> = group
            .overrides
            .iter()
            .map(|(id, severity)| format!("{id}={}", severity.as_str()))
            .collect();
        let props = json!({
            "description": group.description,
            "include": list_text(&group.include),
            "overrides": list_text(&overrides),
            "rule_sha": group.rule_sha,
            "source": source,
            "line": group.line,
        });
        merge_node(db, "RuleGroup", repository_id, &group.id, &props)?;
    }
    Ok(())
}

fn merge_rule(
    db: &mut cypherlite::Connection,
    label: &str,
    repository_id: &str,
    source: &str,
    rule: &Rule,
) -> Result<()> {
    let props = json!({
        "description": rule.description,
        "requires": list_text(&rule.requires),
        "severity": rule.severity.as_str(),
        "cypher": rule.cypher,
        "rule_sha": rule.rule_sha,
        "source": source,
        "line": rule.line,
    });
    merge_node(db, label, repository_id, &rule.id, &props)
}

/// A list property as JSON text: the store keeps lists of strings in String
/// properties, parsed at the boundary (see `deletion.rs`, JSON payloads).
fn list_text(items: &[String]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string())
}

fn merge_node(
    db: &mut cypherlite::Connection,
    label: &str,
    repository_id: &str,
    id: &str,
    props: &Value,
) -> Result<()> {
    let cypher = format!("MERGE (r:{label} {{repository_id: $repo, id: $id}}) SET r += $props");
    let params: Params = store_graph::params(&json!({
        "repo": repository_id,
        "id": id,
        "props": props,
    }));
    store_graph::write(db, &cypher, params)?;
    Ok(())
}

/// The labels rule nodes carry, scoped to a project and keyed by `(repository_id, id)`,
/// plus the per-document marker ([`SOURCE_LABEL`]) that holds each rule document's digest.
pub(super) const RULE_LABELS: &[&str] = &["ConceptRule", "ConstraintRule", "RuleGroup"];

/// The marker node per rule document: `(:RuleSource {repository_id, source, doc_sha})`.
/// It exists even when the document parses to zero rules, so an unchanged empty
/// document is not parsed again at every sync.
pub(super) const SOURCE_LABEL: &str = "RuleSource";

/// Source path to the digest of the text its rules were parsed from, over every
/// recorded document of the project.
pub(super) fn recorded_documents(
    graph: &GraphStore,
    repository_id: &str,
) -> Result<BTreeMap<String, String>> {
    graph.read(|db| {
        let cypher = format!(
            "MATCH (r:{SOURCE_LABEL} {{repository_id: $repo}}) RETURN r.source AS source, r.doc_sha AS sha"
        );
        let mut recorded = BTreeMap::new();
        for row in store_graph::rows(db, &cypher, store_graph::params(&json!({"repo": repository_id})))? {
            if let (Some(source), Some(sha)) = (row["source"].as_str(), row["sha"].as_str()) {
                recorded.insert(source.to_string(), sha.to_string());
            }
        }
        Ok(recorded)
    })
}

/// Replace the rules and the marker of one document in one transaction: the old
/// nodes of `source` are deleted; with `parsed` (the digest and the document),
/// its rules are written and the marker recorded. Without it the document is
/// gone, or unreadable, and keeps nothing.
pub(super) fn replace_document(
    graph: &GraphStore,
    repository_id: &str,
    source: &str,
    parsed: Option<(&str, &RuleDocument)>,
) -> Result<()> {
    graph.transaction(|db| {
        let params = || store_graph::params(&json!({"repo": repository_id, "source": source}));
        for label in RULE_LABELS.iter().chain(std::iter::once(&SOURCE_LABEL)) {
            let cypher = format!(
                "MATCH (r:{label} {{repository_id: $repo, source: $source}}) DETACH DELETE r"
            );
            store_graph::write(db, &cypher, params())?;
        }
        if let Some((sha, document)) = parsed {
            write_rules(db, repository_id, source, document)?;
            let cypher = format!(
                "MERGE (r:{SOURCE_LABEL} {{repository_id: $repo, source: $source}}) SET r.doc_sha = $sha"
            );
            let params = store_graph::params(&json!({"repo": repository_id, "source": source, "sha": sha}));
            store_graph::write(db, &cypher, params)?;
        }
        Ok(())
    })
}

/// The rule set hash: sha256 over the newline-joined, lexicographically sorted
/// `rule_sha` values of every rule node of the project (concepts, constraints,
/// groups). The enrich job's input hash takes it. An empty rule set hashes the
/// empty string.
pub fn rule_set_hash(graph: &GraphStore, repository_id: &str) -> Result<String> {
    let mut shas = graph.read(|db| {
        let mut shas = Vec::new();
        for label in RULE_LABELS {
            let cypher =
                format!("MATCH (r:{label} {{repository_id: $repo}}) RETURN r.rule_sha AS sha");
            for row in store_graph::rows(
                db,
                &cypher,
                store_graph::params(&json!({"repo": repository_id})),
            )? {
                shas.push(row["sha"].as_str().unwrap_or_default().to_string());
            }
        }
        Ok(shas)
    })?;
    shas.sort();
    Ok(graphicle_memory::text::sha256(shas.join("\n").as_bytes()))
}
