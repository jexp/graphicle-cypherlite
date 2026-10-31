//! Rule DAG resolution (architecture-concepts.md §1 and §2).
//!
//! [`resolve`] turns a parsed [`RuleDocument`] and one of its groups into a
//! [`Resolution`]: the rules to run, in execution order, with the effective
//! severity of each. It is a pure function of the rule structs. It performs no
//! store reads or writes and runs no Cypher, so the executor consumes its
//! result directly and tests can exercise it standalone.
//!
//! Semantics:
//! - Concepts form the DAG. `requires` names concepts by exact id or by a
//!   trailing-`*` prefix. A wildcard never matches the rule that declares it.
//! - Constraints are sinks: they may require concepts, nothing may require
//!   them, and they run after every concept.
//! - A group selects rules by `include` patterns, then takes the transitive
//!   `requires` closure, so a dependency runs even when the group omits it.
//! - Group `overrides` set effective severities. An exact id beats any
//!   wildcard, and a longer prefix beats a shorter one. Rules without an
//!   override keep their declared severity.
//! - [`Resolution::skip_after`] applies failures at execution time: a failed
//!   concept moves its transitive dependents from `order` to `skipped`.

use crate::arch_rules::{Rule, RuleDocument, Severity};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Why a group cannot be resolved. Each variant names the rules involved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// No group with this id exists in the document.
    UnknownGroup(String),
    /// `rule` requires `requires`, which matches no concept.
    UnresolvedRequire { rule: String, requires: String },
    /// `rule` requires `requires`, which names a constraint (a sink).
    RequiresConstraint { rule: String, requires: String },
    /// `group` includes `pattern`, which matches no rule.
    UnresolvedInclude { group: String, pattern: String },
    /// `group` overrides `pattern`, which matches no rule.
    UnresolvedOverride { group: String, pattern: String },
    /// `group` lists the same override pattern more than once.
    DuplicateOverride { group: String, pattern: String },
    /// The listed rules are on one or more dependency cycles. Every rule that
    /// lies on a cycle is named; rules that only depend on a cycle are not.
    Cycle { rules: Vec<String> },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::UnknownGroup(group) => write!(f, "no rule group '{group}'"),
            ResolveError::UnresolvedRequire { rule, requires } => write!(
                f,
                "rule '{rule}' requires '{requires}', which matches no concept"
            ),
            ResolveError::RequiresConstraint { rule, requires } => write!(
                f,
                "rule '{rule}' requires '{requires}', which is a constraint; constraints are sinks and cannot be required"
            ),
            ResolveError::UnresolvedInclude { group, pattern } => write!(
                f,
                "group '{group}' includes '{pattern}', which matches no rule"
            ),
            ResolveError::UnresolvedOverride { group, pattern } => write!(
                f,
                "group '{group}' overrides '{pattern}', which matches no rule"
            ),
            ResolveError::DuplicateOverride { group, pattern } => {
                write!(f, "group '{group}' overrides '{pattern}' more than once")
            }
            ResolveError::Cycle { rules } => {
                let names: Vec<String> = rules.iter().map(|rule| format!("'{rule}'")).collect();
                write!(f, "rule cycle through {}", names.join(", "))
            }
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<ResolveError> for crate::Error {
    fn from(error: ResolveError) -> Self {
        crate::Error::bad(error)
    }
}

/// The plan for one group, ready for the executor.
///
/// `order` lists the rules still to run: concepts in dependency order, then
/// constraints in document order. `skipped` lists rules that a failure has
/// removed from the plan. `severities` maps every rule in `order` and
/// `skipped` to its effective severity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    pub group: String,
    pub order: Vec<String>,
    pub skipped: Vec<String>,
    pub severities: BTreeMap<String, Severity>,
    /// Direct dependents of each plan member, used to propagate failures.
    dependents: BTreeMap<String, Vec<String>>,
}

impl Resolution {
    /// Apply concept failures: each failed id is dropped from `order`, and its
    /// transitive dependents move from `order` to `skipped` in plan order.
    /// Ids that are not in the plan are ignored.
    pub fn skip_after(mut self, failed: &[&str]) -> Resolution {
        let mut doomed: BTreeSet<String> = BTreeSet::new();
        let mut stack: Vec<String> = failed.iter().map(|id| id.to_string()).collect();
        while let Some(id) = stack.pop() {
            for dependent in self.dependents.get(&id).into_iter().flatten() {
                if doomed.insert(dependent.clone()) {
                    stack.push(dependent.clone());
                }
            }
        }
        let mut remaining = Vec::new();
        for id in std::mem::take(&mut self.order) {
            if failed.contains(&id.as_str()) {
                continue;
            }
            if doomed.contains(&id) {
                self.skipped.push(id);
            } else {
                remaining.push(id);
            }
        }
        self.order = remaining;
        self
    }

    /// The executor's contract as JSON:
    /// `{"order": [...], "skipped": [...], "severities": {id: SEVERITY}}`.
    pub fn to_contract(&self) -> Value {
        let severities: Map<String, Value> = self
            .severities
            .iter()
            .map(|(id, severity)| (id.clone(), json!(severity.as_str())))
            .collect();
        json!({
            "order": self.order,
            "skipped": self.skipped,
            "severities": Value::Object(severities),
        })
    }
}

/// Resolve one group of `document` into its execution plan.
pub fn resolve(document: &RuleDocument, group_id: &str) -> Result<Resolution, ResolveError> {
    let group = document
        .groups
        .iter()
        .find(|group| group.id == group_id)
        .ok_or_else(|| ResolveError::UnknownGroup(group_id.to_string()))?;

    let rules: Vec<&Rule> = document
        .concepts
        .iter()
        .chain(&document.constraints)
        .collect();
    let by_id: BTreeMap<&str, &Rule> = rules.iter().map(|rule| (rule.id.as_str(), *rule)).collect();
    let concept_ids: BTreeSet<&str> = document
        .concepts
        .iter()
        .map(|rule| rule.id.as_str())
        .collect();
    let constraint_ids: BTreeSet<&str> = document
        .constraints
        .iter()
        .map(|rule| rule.id.as_str())
        .collect();

    // Expand every `requires` into concept ids. Wildcards and exact names both
    // resolve here, so the plan only ever holds concrete ids.
    let mut requires_of: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for rule in &rules {
        let mut needs = BTreeSet::new();
        for item in &rule.requires {
            let unresolved = || ResolveError::UnresolvedRequire {
                rule: rule.id.clone(),
                requires: item.clone(),
            };
            match item.strip_suffix('*') {
                Some(prefix) => {
                    let found: Vec<&str> = concept_ids
                        .iter()
                        .copied()
                        .filter(|id| id.starts_with(prefix) && *id != rule.id.as_str())
                        .collect();
                    if found.is_empty() {
                        return Err(unresolved());
                    }
                    needs.extend(found);
                }
                None if concept_ids.contains(item.as_str()) => {
                    needs.insert(by_id[item.as_str()].id.as_str());
                }
                None if constraint_ids.contains(item.as_str()) => {
                    return Err(ResolveError::RequiresConstraint {
                        rule: rule.id.clone(),
                        requires: item.clone(),
                    });
                }
                None => return Err(unresolved()),
            }
        }
        requires_of.insert(rule.id.as_str(), needs);
    }

    // Select by include patterns, then close over requires so every
    // dependency of a selected rule is part of the plan.
    let mut selected: BTreeSet<&str> = BTreeSet::new();
    for pattern in &group.include {
        let found: Vec<&str> = rules
            .iter()
            .map(|rule| rule.id.as_str())
            .filter(|id| matches(pattern, id))
            .collect();
        if found.is_empty() {
            return Err(ResolveError::UnresolvedInclude {
                group: group.id.clone(),
                pattern: pattern.clone(),
            });
        }
        selected.extend(found);
    }
    let mut members = selected.clone();
    let mut stack: Vec<&str> = selected.into_iter().collect();
    while let Some(id) = stack.pop() {
        for &need in requires_of.get(id).into_iter().flatten() {
            if members.insert(need) {
                stack.push(need);
            }
        }
    }

    let overrides = checked_overrides(group, &rules)?;
    let severity_of = |id: &str| -> Severity {
        let declared = by_id[id].severity;
        overrides
            .iter()
            .filter(|(pattern, _)| matches(pattern, id))
            .max_by_key(|(pattern, _)| specificity(pattern))
            .map(|(_, severity)| *severity)
            .unwrap_or(declared)
    };

    // Dependents: the reverse of requires, restricted to plan members.
    let mut reverse: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for &member in &members {
        for &need in requires_of.get(member).into_iter().flatten() {
            reverse.entry(need).or_default().push(member);
        }
    }

    let concept_order = topological_concepts(document, &requires_of, &members)?;
    let constraint_order: Vec<&str> = document
        .constraints
        .iter()
        .map(|rule| rule.id.as_str())
        .filter(|id| members.contains(id))
        .collect();

    let order: Vec<String> = concept_order
        .iter()
        .chain(&constraint_order)
        .map(|id| id.to_string())
        .collect();
    let severities = order
        .iter()
        .map(|id| (id.clone(), severity_of(id)))
        .collect();
    let dependents = members
        .iter()
        .map(|&member| {
            let dependent_ids = reverse
                .get(member)
                .into_iter()
                .flatten()
                .map(|id| id.to_string())
                .collect();
            (member.to_string(), dependent_ids)
        })
        .collect();

    Ok(Resolution {
        group: group.id.clone(),
        order,
        skipped: Vec::new(),
        severities,
        dependents,
    })
}

/// Validate a group's overrides: each must match a rule and appear once.
fn checked_overrides<'a>(
    group: &'a crate::arch_rules::Group,
    rules: &[&Rule],
) -> Result<Vec<(&'a str, Severity)>, ResolveError> {
    let mut seen = BTreeSet::new();
    let mut checked = Vec::new();
    for (pattern, severity) in &group.overrides {
        if !seen.insert(pattern.as_str()) {
            return Err(ResolveError::DuplicateOverride {
                group: group.id.clone(),
                pattern: pattern.clone(),
            });
        }
        if !rules.iter().any(|rule| matches(pattern, &rule.id)) {
            return Err(ResolveError::UnresolvedOverride {
                group: group.id.clone(),
                pattern: pattern.clone(),
            });
        }
        checked.push((pattern.as_str(), *severity));
    }
    Ok(checked)
}

/// Kahn's algorithm over the concept members, ready rules taken in document
/// order. Rules left over after the sort are blocked, and the cycle members
/// among them are reported.
fn topological_concepts<'a>(
    document: &'a RuleDocument,
    requires_of: &BTreeMap<&'a str, BTreeSet<&'a str>>,
    members: &BTreeSet<&'a str>,
) -> Result<Vec<&'a str>, ResolveError> {
    let index: BTreeMap<&str, usize> = document
        .concepts
        .iter()
        .enumerate()
        .map(|(at, rule)| (rule.id.as_str(), at))
        .collect();
    let concepts: Vec<&str> = document
        .concepts
        .iter()
        .map(|rule| rule.id.as_str())
        .filter(|id| members.contains(id))
        .collect();
    let pending: BTreeMap<&str, BTreeSet<&str>> = concepts
        .iter()
        .map(|id| (*id, requires_of.get(id).cloned().unwrap_or_default()))
        .collect();

    let mut waiting = pending.clone();
    let mut ready: BTreeSet<(usize, &str)> = waiting
        .iter()
        .filter(|(_, needs)| needs.is_empty())
        .map(|(id, _)| (index[id], *id))
        .collect();
    let mut order = Vec::new();
    while let Some(next) = ready.pop_first() {
        let id = next.1;
        order.push(id);
        waiting.remove(id);
        for (dependent, needs) in waiting.iter_mut() {
            if needs.remove(id) && needs.is_empty() {
                ready.insert((index[dependent], *dependent));
            }
        }
    }

    if waiting.is_empty() {
        return Ok(order);
    }
    let on_cycle: Vec<String> = waiting
        .keys()
        .filter(|id| reaches(&pending, id, id))
        .map(|id| id.to_string())
        .collect();
    Err(ResolveError::Cycle { rules: on_cycle })
}

/// Whether `target` can be reached from `from` by following requires edges.
fn reaches(graph: &BTreeMap<&str, BTreeSet<&str>>, from: &str, target: &str) -> bool {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = graph.get(from).into_iter().flatten().copied().collect();
    while let Some(id) = stack.pop() {
        if id == target {
            return true;
        }
        if seen.insert(id) {
            stack.extend(graph.get(id).into_iter().flatten().copied());
        }
    }
    false
}

/// Whether a rule id or a trailing-`*` pattern matches `id`.
fn matches(pattern: &str, id: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => id.starts_with(prefix),
        None => pattern == id,
    }
}

/// Ranks an override pattern: exact ids beat wildcards, longer prefixes beat
/// shorter ones.
fn specificity(pattern: &str) -> (u8, usize) {
    match pattern.strip_suffix('*') {
        Some(prefix) => (0, prefix.len()),
        None => (1, pattern.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch_rules::parse;

    fn concept(id: &str, requires: &[&str], severity: &str) -> String {
        format!(
            "````concept\nid: {id}\nrequires: [{}]\nseverity: {severity}\n```cypher\nRETURN 1\n```\n````\n",
            requires.join(", ")
        )
    }

    fn constraint(id: &str, requires: &[&str], severity: &str) -> String {
        format!(
            "````constraint\nid: {id}\nrequires: [{}]\nseverity: {severity}\n```cypher\nRETURN 1\n```\n````\n",
            requires.join(", ")
        )
    }

    fn group(id: &str, include: &[&str], overrides: &[&str]) -> String {
        format!(
            "````group\nid: {id}\ninclude: [{}]\noverrides: [{}]\n````\n",
            include.join(", "),
            overrides.join(", ")
        )
    }

    fn document(blocks: &[String]) -> RuleDocument {
        parse(&blocks.join("\n")).expect("test document parses")
    }

    fn ids(items: &[String]) -> Vec<&str> {
        items.iter().map(String::as_str).collect()
    }

    #[test]
    fn resolves_wildcards_overrides_and_topological_order() {
        let doc = document(&[
            concept("arch:Module/Core", &[], "INFO"),
            concept("arch:Layer/Persistence", &["arch:Module/*"], "MINOR"),
            concept("arch:Layer/Service", &["arch:Layer/Persistence"], "MAJOR"),
            constraint("arch:NoCycles", &["arch:Layer/Service"], "BLOCKER"),
            group(
                "arch:default",
                &["arch:*"],
                &["arch:Layer/*=MAJOR", "arch:Layer/Service=CRITICAL"],
            ),
        ]);
        let plan = resolve(&doc, "arch:default").unwrap();
        assert_eq!(
            plan.order,
            vec![
                "arch:Module/Core",
                "arch:Layer/Persistence",
                "arch:Layer/Service",
                "arch:NoCycles",
            ]
        );
        assert!(plan.skipped.is_empty());
        assert_eq!(plan.severities["arch:Module/Core"], Severity::Info);
        // The wildcard override raises a MINOR concept to MAJOR.
        assert_eq!(plan.severities["arch:Layer/Persistence"], Severity::Major);
        // An exact override beats the wildcard that also matches it.
        assert_eq!(plan.severities["arch:Layer/Service"], Severity::Critical);
        assert_eq!(plan.severities["arch:NoCycles"], Severity::Blocker);
    }

    #[test]
    fn group_closure_pulls_in_required_concepts() {
        let doc = document(&[
            concept("arch:Module/Core", &[], "INFO"),
            concept("arch:Layer/Persistence", &["arch:Module/*"], "MINOR"),
            concept("arch:Layer/Service", &["arch:Layer/Persistence"], "MAJOR"),
            constraint("arch:NoCycles", &[], "BLOCKER"),
            group("arch:layers", &["arch:Layer/Service"], &[]),
        ]);
        let plan = resolve(&doc, "arch:layers").unwrap();
        assert_eq!(
            plan.order,
            vec![
                "arch:Module/Core",
                "arch:Layer/Persistence",
                "arch:Layer/Service"
            ]
        );
    }

    #[test]
    fn wildcard_never_matches_the_declaring_rule() {
        let doc = document(&[
            concept("arch:Module/Core", &[], "INFO"),
            concept("arch:Module/Other", &["arch:Module/*"], "INFO"),
            group("g", &["arch:*"], &[]),
        ]);
        let plan = resolve(&doc, "g").unwrap();
        assert_eq!(plan.order, vec!["arch:Module/Core", "arch:Module/Other"]);
    }

    #[test]
    fn unresolvable_requires_name_both_rules() {
        let wildcard = document(&[
            concept("arch:Layer/Service", &["arch:Module/*"], "INFO"),
            group("g", &["arch:*"], &[]),
        ]);
        assert_eq!(
            resolve(&wildcard, "g").unwrap_err(),
            ResolveError::UnresolvedRequire {
                rule: "arch:Layer/Service".into(),
                requires: "arch:Module/*".into(),
            }
        );

        let exact = document(&[
            concept("arch:Layer/Service", &["arch:Missing"], "INFO"),
            group("g", &["arch:*"], &[]),
        ]);
        let error = resolve(&exact, "g").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("'arch:Layer/Service'") && text.contains("'arch:Missing'"));
    }

    #[test]
    fn requiring_a_constraint_is_rejected() {
        let doc = document(&[
            constraint("arch:NoCycles", &[], "BLOCKER"),
            concept("arch:Layer/Service", &["arch:NoCycles"], "INFO"),
            group("g", &["arch:*"], &[]),
        ]);
        assert!(matches!(
            resolve(&doc, "g"),
            Err(ResolveError::RequiresConstraint { .. })
        ));
    }

    #[test]
    fn cycle_names_every_rule_on_it_and_not_its_dependents() {
        let doc = document(&[
            concept("arch:A", &["arch:B"], "INFO"),
            concept("arch:B", &["arch:C"], "INFO"),
            concept("arch:C", &["arch:A"], "INFO"),
            concept("arch:Downstream", &["arch:A"], "INFO"),
            concept("arch:Free", &[], "INFO"),
            group("g", &["arch:*"], &[]),
        ]);
        assert_eq!(
            resolve(&doc, "g").unwrap_err(),
            ResolveError::Cycle {
                rules: vec!["arch:A".into(), "arch:B".into(), "arch:C".into()],
            }
        );

        let self_loop = document(&[
            concept("arch:Loop", &["arch:Loop"], "INFO"),
            group("g", &["arch:*"], &[]),
        ]);
        assert_eq!(
            resolve(&self_loop, "g").unwrap_err(),
            ResolveError::Cycle {
                rules: vec!["arch:Loop".into()]
            }
        );
    }

    #[test]
    fn failed_concept_skips_dependents_transitively() {
        let doc = document(&[
            concept("arch:A", &[], "INFO"),
            concept("arch:B", &["arch:A"], "INFO"),
            concept("arch:C", &["arch:B"], "INFO"),
            concept("arch:D", &[], "INFO"),
            constraint("arch:K", &["arch:C"], "MAJOR"),
            group("g", &["arch:*"], &[]),
        ]);
        let plan = resolve(&doc, "g").unwrap();
        assert_eq!(
            plan.order,
            vec!["arch:A", "arch:B", "arch:C", "arch:D", "arch:K"]
        );

        // The executor reports A failed. B, C and the constraint K depend on
        // it, directly or through C, so they are skipped. D is untouched.
        let after = plan.skip_after(&["arch:A"]);
        assert_eq!(after.order, vec!["arch:D"]);
        assert_eq!(ids(&after.skipped), vec!["arch:B", "arch:C", "arch:K"]);
        assert_eq!(after.severities["arch:K"], Severity::Major);
    }

    #[test]
    fn overrides_matching_nothing_and_duplicates_are_errors() {
        let doc = document(&[
            concept("arch:A", &[], "INFO"),
            group("g", &["arch:*"], &["arch:Typo=MAJOR"]),
        ]);
        assert!(matches!(
            resolve(&doc, "g"),
            Err(ResolveError::UnresolvedOverride { .. })
        ));

        let twice = document(&[
            concept("arch:A", &[], "INFO"),
            group("g", &["arch:*"], &["arch:A=MAJOR", "arch:A=MINOR"]),
        ]);
        assert!(matches!(
            resolve(&twice, "g"),
            Err(ResolveError::DuplicateOverride { .. })
        ));
    }

    #[test]
    fn unknown_group_and_empty_include_are_errors() {
        let doc = document(&[
            concept("arch:A", &[], "INFO"),
            group("g", &["arch:Nope*"], &[]),
        ]);
        assert_eq!(
            resolve(&doc, "missing").unwrap_err(),
            ResolveError::UnknownGroup("missing".into())
        );
        assert!(matches!(
            resolve(&doc, "g"),
            Err(ResolveError::UnresolvedInclude { .. })
        ));
    }

    #[test]
    fn contract_json_has_the_executor_shape() {
        let doc = document(&[
            concept("arch:A", &[], "MINOR"),
            constraint("arch:K", &["arch:A"], "BLOCKER"),
            group("g", &["arch:*"], &[]),
        ]);
        let contract = resolve(&doc, "g").unwrap().to_contract();
        assert_eq!(
            contract,
            json!({
                "order": ["arch:A", "arch:K"],
                "skipped": [],
                "severities": {"arch:A": "MINOR", "arch:K": "BLOCKER"},
            })
        );
    }
}
