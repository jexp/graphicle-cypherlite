//! The enrich executor (architecture-concepts.md §1, §2 and §5): applies a
//! project's concept rules to one recorded episode, topologically, one graph
//! transaction per rule.
//!
//! The executor does not parse rule bodies — a concept *is* its Cypher, run
//! as written with `$repo` (the repository id), `$episode` and `$rule` (the
//! rule id) as parameters. What it adds around them:
//!
//! - **Order and failure semantics**: the rules come back from the store as
//!   a [`RuleDocument`] and go through [`super::dag::resolve`], so they run
//!   in dependency order with the groups' severity overrides applied. A rule
//!   whose query fails is rolled back whole (its own transaction), reported
//!   under `failed`, and its transitive dependents move to `skipped`
//!   ([`super::dag::Resolution::skip_after`]) — never a half-applied state.
//!   Constraints
//!   are resolved and carried (their effective severities are part of the
//!   result) but never executed here; a later node evaluates them against
//!   the enriched graph.
//! - **Provenance**: [`super::provenance`] brackets each rule's query with
//!   whole-store snapshots inside the rule's own transaction, records
//!   exactly what it wrote, and rolls those writes back on demand; see its
//!   module doc for the cost and the limits.
//! - **Idempotence**: a rule whose query is MERGE-based writes nothing the
//!   second time, so re-applying a rule set to the same episode — a
//!   replayed job, a retry — records nothing new. The application record
//!   itself MERGEs on `(repository_id, rule, episode)`.
//!
//! Which rules run: every group the project declared, in group-id order, a
//! rule applied once across the groups that select it; a project that
//! declared no group runs every rule (documents without groups — the common
//! case — clearly intend all of them).
//!
//! The run's result is typed ([`EnrichReport`], [`Plan`]): the executor
//! builds it and serializes it as the `enrich` job's result, and the
//! constraint evaluator and the analyze report read the same structs back —
//! the serialized shape is pinned in [`super::plan`].
use super::model::{Rule, RuleDocument, Severity};
use super::plan::{AppliedRule, EnrichReport, FailedRule, Plan};
use super::provenance;
use super::{Group, rule_set_hash};
use crate::Result;
use crate::store_graph::{self, GraphStore};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// The group the executor resolves when the project declared none: every
/// rule, no overrides. Its id only appears in the result's plan reports.
const DEFAULT_GROUP: &str = "default";

/// Apply the project's concept rules to `episode` (architecture-concepts.md
/// §1.5: enrichment is the post-processing step of a recorded projection).
/// `still_leasing` is checked before every rule, so a job whose lease was
/// cancelled or taken over stops with that error. See the module doc for
/// order, failure and provenance semantics; the result is the job's, as
/// [`EnrichReport`] (serialized by [`EnrichReport::to_value`]):
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
) -> Result<EnrichReport> {
    let document = rule_document(graph, repository_id)?;
    let rule_set = rule_set_hash(graph, repository_id)?;
    let Some(document) = document else {
        return Ok(EnrichReport {
            kind: "enrich".into(),
            episode: episode.into(),
            rule_set,
            plans: Vec::new(),
            applied: 0,
            failed: 0,
            skipped: 0,
            constraints: None,
        });
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
    // One plan per declared group, in group-id order; with no group, one
    // plan of every rule.
    let mut run = Run {
        graph,
        repository_id,
        episode,
        document: &document,
        concepts: &concepts,
        constraint_ids: &constraint_ids,
        applied: BTreeSet::new(),
        still_leasing,
    };
    let mut plans = Vec::new();
    if document.groups.is_empty() {
        let every = with_default_group(&document);
        run.document = &every;
        plans.push(run.plan(DEFAULT_GROUP)?);
    } else {
        let mut groups: Vec<&str> = document.groups.iter().map(|g| g.id.as_str()).collect();
        groups.sort_unstable();
        for group in groups {
            plans.push(run.plan(group)?);
        }
    }
    let total = |rules: fn(&Plan) -> usize| plans.iter().map(rules).sum::<usize>();
    Ok(EnrichReport {
        kind: "enrich".into(),
        episode: episode.into(),
        rule_set,
        applied: total(|plan| plan.applied.len()),
        failed: total(|plan| plan.failed.len()),
        skipped: total(|plan| plan.skipped.len()),
        plans,
        constraints: None,
    })
}

/// What one run's plans share: the store and episode they work on, the
/// document as resolved, and the lease check before every rule. `applied`
/// carries the applied ids across the groups of one run.
struct Run<'a> {
    graph: &'a GraphStore,
    repository_id: &'a str,
    episode: &'a str,
    document: &'a RuleDocument,
    concepts: &'a BTreeMap<&'a str, &'a Rule>,
    constraint_ids: &'a BTreeSet<&'a str>,
    applied: BTreeSet<String>,
    still_leasing: &'a dyn Fn() -> Result<()>,
}

impl Run<'_> {
    /// Resolve one group and run its plan. Rules an earlier group already
    /// applied in this run are not run again (their queries are
    /// MERGE-idempotent either way; skipping keeps one failure semantics per
    /// run).
    fn plan(&mut self, group: &str) -> Result<Plan> {
        let resolution = match super::dag::resolve(self.document, group) {
            Ok(resolution) => resolution,
            Err(error) => {
                return Ok(Plan {
                    group: group.to_string(),
                    error: Some(error.to_string()),
                    applied: Vec::new(),
                    failed: Vec::new(),
                    skipped: Vec::new(),
                    severities: BTreeMap::new(),
                });
            }
        };
        let mut resolution = resolution;
        // The plan as resolved; failures below remove members from it again.
        let mut queue: VecDeque<String> = resolution.order.iter().cloned().collect();
        let mut applied = Vec::new();
        let mut failed = Vec::new();
        while let Some(id) = queue.pop_front() {
            // Dropped by an earlier failure's skip_after, a constraint (the
            // later node evaluates those), or an earlier group of this run.
            if !resolution.order.contains(&id)
                || self.constraint_ids.contains(id.as_str())
                || self.applied.contains(&id)
            {
                continue;
            }
            let Some(rule) = self.concepts.get(id.as_str()) else {
                continue;
            };
            (self.still_leasing)()?;
            match apply_rule(self.graph, self.repository_id, self.episode, rule) {
                Ok(report) => {
                    self.applied.insert(id.clone());
                    applied.push(report);
                }
                Err(error) => {
                    failed.push(FailedRule {
                        rule: id.clone(),
                        error: error.message,
                    });
                    resolution = resolution.skip_after(&[id.as_str()]);
                }
            }
        }
        Ok(Plan {
            group: group.to_string(),
            error: None,
            applied,
            failed,
            skipped: resolution.skipped,
            severities: resolution.severities,
        })
    }
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
pub(super) fn rule_document(
    graph: &GraphStore,
    repository_id: &str,
) -> Result<Option<RuleDocument>> {
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
                let requires: Vec<String> =
                    serde_json::from_str(store_graph::s(&row, "requires")).unwrap_or_default();
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
/// between them becomes the application's provenance (see
/// [`provenance`]). A failure rolls the transaction back — the rule leaves
/// nothing behind — and surfaces as the caller's `failed` entry.
fn apply_rule(
    graph: &GraphStore,
    repository_id: &str,
    episode: &str,
    rule: &Rule,
) -> Result<AppliedRule> {
    graph.transaction(|db| {
        let before = provenance::snapshot(db)?;
        store_graph::write(
            db,
            &rule.cypher,
            store_graph::params(&json!({
                "repo": repository_id, "episode": episode, "rule": rule.id,
            })),
        )?;
        let delta = provenance::diff(&before, &provenance::snapshot(db)?);
        provenance::write_application(db, repository_id, episode, rule, &delta)?;
        Ok(AppliedRule {
            rule: rule.id.clone(),
            labels: delta.labels.len(),
            relationships: delta.created_rels.len(),
            properties: delta.node_props.len() + delta.rel_props.len(),
            nodes: delta.created_nodes.len(),
        })
    })
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
    use crate::arch_rules::tests::{REPO, SHARED, add_symbols, count, rules_graph};
    use crate::store_graph::Params;
    use serde_json::Value;

    fn enrich(graph: &GraphStore, episode: &str) -> Value {
        let leasing = || Ok(());
        super::enrich(graph, REPO, episode, &leasing)
            .unwrap()
            .to_value()
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

    #[test]
    fn applies_concepts_in_resolution_order_and_carries_constraints() {
        let graph = rules_graph(SHARED, &["db_one", "db_two", "plain"]);
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
            count(
                &graph,
                "MATCH (s:Symbol) WHERE s.arch_confidence = 0.8 RETURN count(s) AS n"
            ),
            3
        );
        assert_eq!(
            count(
                &graph,
                "MATCH (l:Layer {name: 'Persistence'}) RETURN count(l) AS n"
            ),
            1
        );
        // One application record per concept and episode.
        assert_eq!(
            count(
                &graph,
                "MATCH (a:RuleApplication {episode: 'ep-1'}) RETURN count(a) AS n"
            ),
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
            skipped
                .iter()
                .filter_map(|s| s.as_str())
                .collect::<Vec<_>>(),
            vec!["arch:Test/Dependent", "arch:Test/NoCycles"],
            "{report:#}"
        );
        // Nothing of the failed rule is left behind.
        assert_eq!(
            count(
                &graph,
                "MATCH (s) WHERE s.b_mark IS NOT NULL RETURN count(s) AS n"
            ),
            0
        );
        assert_eq!(
            count(
                &graph,
                "MATCH (a:RuleApplication {rule: 'arch:Test/Connect'}) RETURN count(a) AS n"
            ),
            0
        );
        // Its dependency and the independent rule both applied.
        assert_eq!(
            count(&graph, "MATCH (s:Symbol:Persistence) RETURN count(s) AS n"),
            2
        );
        assert_eq!(
            count(
                &graph,
                "MATCH (s:Symbol) WHERE s.d_mark = 'x' RETURN count(s) AS n"
            ),
            3
        );
    }

    /// The failure probe's store: [`PROBE_RULES`] over an Object node whose
    /// key the failing CREATE duplicates.
    fn graph_with_probe() -> GraphStore {
        let graph = rules_graph(PROBE_RULES, &["db_one", "db_two", "plain"]);
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
        let graph = rules_graph(SHARED, &["db_one", "db_two", "plain"]);
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
    fn each_episode_enriches_on_its_own_and_keeps_its_applications() {
        let graph = rules_graph(SHARED, &["db_one", "db_two"]);
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
            count(
                &graph,
                "MATCH (a:RuleApplication {episode: 'ep-1'}) RETURN count(a) AS n"
            ),
            4
        );
        assert_eq!(
            count(
                &graph,
                "MATCH (a:RuleApplication {episode: 'ep-2'}) RETURN count(a) AS n"
            ),
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
        let graph = rules_graph(rules, &["one"]);
        let report = enrich(&graph, "ep-1");
        assert_eq!(report["applied"], json!(1), "{report:#}");
        let plan = &report["plans"][0];
        assert_eq!(plan["group"], json!("arch:picked"));
        assert_eq!(
            plan["severities"]["arch:A"],
            json!("CRITICAL"),
            "{report:#}"
        );
        // The rule the group does not include did not run.
        assert_eq!(
            count(
                &graph,
                "MATCH (s) WHERE s.b_prop IS NOT NULL RETURN count(s) AS n"
            ),
            0
        );
        assert_eq!(
            count(&graph, "MATCH (s:Symbol:Grouped) RETURN count(s) AS n"),
            1
        );
    }

    #[test]
    fn has_rules_reads_the_stored_rule_nodes() {
        let with = rules_graph(SHARED, &[]);
        assert!(has_rules(&with, REPO).unwrap());
        assert!(!has_rules(&with, "other").unwrap());
        let without = GraphStore::open_in_memory().unwrap();
        assert!(!has_rules(&without, REPO).unwrap());
    }
}
