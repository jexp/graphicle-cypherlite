//! Storage of a parsed rule document: `(:ConceptRule)`, `(:ConstraintRule)`
//! and `(:RuleGroup)` nodes scoped to one project, keyed by
//! `(repository_id, id)`. Each node carries a `rule_sha`: a digest of the
//! rule's content, so re-parsing identical text yields the same value. The
//! store is the same whichever frontend read the document.
use super::model::{Rule, RuleDocument};
use crate::store_graph::{self, GraphStore, Params};
use crate::Result;
use serde_json::{Value, json};

/// Write the document's rules as nodes scoped to `repository_id`, merged on
/// `(repository_id, id)` so a re-parse updates in place. `source` is the
/// document's path, kept as evidence of where a rule was declared.
pub fn store(
    graph: &GraphStore,
    repository_id: &str,
    source: &str,
    document: &RuleDocument,
) -> Result<()> {
    graph.transaction(|db| {
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
    })
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
    let cypher = format!(
        "MERGE (r:{label} {{repository_id: $repo, id: $id}}) SET r += $props"
    );
    let params: Params = store_graph::params(&json!({
        "repo": repository_id,
        "id": id,
        "props": props,
    }));
    store_graph::write(db, &cypher, params)?;
    Ok(())
}
