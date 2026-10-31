//! The report of `graphicle analyze` (architecture-concepts.md §2, the CI
//! surface): the episode's enrich result and its violation rows, with the
//! exit status a CI job gates on. Pure functions of their inputs; `analyze`
//! reads the store and calls these.
//!
//! Exit status: 2 when the run has errors (a malformed rule document, an
//! unknown group, a rule that failed to run), else 1 when a violation at or
//! above `--min-severity` exists, else 0. Without `--min-severity` nothing
//! gates: every violation is reported and the status stays 0. A run whose
//! enrich job has not finished is state `waiting` and exits 0; it is neither
//! a violation nor an error.
use super::model::Severity;
use super::plan::{EnrichReport, Status};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

/// One violation of the episode: a constraint's result row.
#[derive(Clone, Debug, PartialEq)]
pub struct Violation {
    pub rule: String,
    pub severity: Severity,
    pub path: Option<String>,
    pub line: Option<i64>,
}

impl Violation {
    /// The evidence location: `path:line`, `path`, or empty.
    pub fn evidence(&self) -> String {
        match (&self.path, self.line) {
            (Some(path), Some(line)) => format!("{path}:{line}"),
            (Some(path), None) => path.clone(),
            _ => String::new(),
        }
    }
}

/// The violations of store rows holding `rule`, `severity`, `path` and
/// `line`. A row without a rule or a known severity is dropped.
pub fn violations(rows: &[Value]) -> Vec<Violation> {
    rows.iter()
        .filter_map(|row| {
            Some(Violation {
                rule: row["rule"].as_str()?.to_string(),
                severity: Severity::parse(row["severity"].as_str()?)?,
                path: row["path"].as_str().map(str::to_string),
                line: row["line"].as_i64(),
            })
        })
        .collect()
}

/// The exit status of a run with `errors` errors and these `violations`,
/// gated at `min` (see the module documentation).
pub fn exit_code(errors: usize, violations: &[Violation], min: Option<Severity>) -> i32 {
    if errors > 0 {
        return 2;
    }
    match min {
        Some(min) if violations.iter().any(|v| v.severity >= min) => 1,
        _ => 0,
    }
}

/// The report of a finished enrich run. `job` is the enrich job's result
/// (see [`crate::arch_rules::executor::enrich`] and
/// [`crate::arch_rules::plan`]); `violations` are the episode's rows. With
/// `group`, only that group's plan, its constraints and its violations are
/// reported; a group the project does not declare is an error.
pub fn complete(
    episode: &str,
    rule_set: &str,
    job: &EnrichReport,
    violations: &[Violation],
    group: Option<&str>,
    min: Option<Severity>,
    document_errors: &[String],
) -> Value {
    let mut errors = document_errors.to_vec();
    let mut concepts = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = 0usize;
    let mut members = BTreeSet::new();
    let mut found_group = group.is_none();
    for plan in &job.plans {
        let name = plan.group.as_str();
        if group == Some(name) {
            found_group = true;
        } else if group.is_some() {
            continue;
        }
        if let Some(error) = &plan.error {
            errors.push(format!("group '{name}': {error}"));
        }
        for item in &plan.applied {
            members.insert(item.rule.clone());
            concepts.push(json!({
                "rule": item.rule,
                "group": name,
                "labels": item.labels,
                "relationships": item.relationships,
                "properties": item.properties,
                "nodes": item.nodes,
            }));
        }
        for item in &plan.failed {
            members.insert(item.rule.clone());
            failed += 1;
            errors.push(format!("rule '{}' failed: {}", item.rule, item.error));
        }
        for id in &plan.skipped {
            members.insert(id.clone());
            skipped.push(id.clone());
        }
        members.extend(plan.severities.keys().cloned());
    }
    if let (false, Some(group)) = (found_group, group) {
        errors.push(format!("no rule group '{group}'"));
    }
    let mut constraints = Vec::new();
    for result in job.constraints.as_ref().into_iter().flat_map(|c| &c.results) {
        if !members.contains(&result.rule) {
            continue;
        }
        if result.status == Status::Error {
            errors.push(format!(
                "rule '{}' failed: {}",
                result.rule,
                result.error.as_deref().unwrap_or_default()
            ));
        }
        constraints.push(json!({
            "rule": result.rule,
            "status": result.status,
            "severity": result.severity,
            "violations": result.violations,
        }));
    }
    let mut selected: Vec<Violation> = violations
        .iter()
        .filter(|v| members.contains(&v.rule))
        .cloned()
        .collect();
    selected.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.rule.cmp(&b.rule))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    let exit = exit_code(errors.len(), &selected, min);
    let by_severity = by_severity(selected.iter().map(|v| v.severity));
    let (concept_count, skipped_count, violation_count) =
        (concepts.len(), skipped.len(), selected.len());
    json!({
        "state": "complete",
        "detail": null,
        "episode": episode,
        "rule_set": rule_set,
        "group": group,
        "min_severity": min.map(Severity::as_str),
        "concepts": concepts,
        "constraints": constraints,
        "skipped": skipped,
        "violations": selected.iter().map(violation_json).collect::<Vec<_>>(),
        "counts": {
            "concepts": concept_count,
            "failed": failed,
            "skipped": skipped_count,
            "violations": violation_count,
            "by_severity": by_severity,
        },
        "errors": errors,
        "exit": exit,
    })
}

/// A run that waits for queued or running work (the code graph or the enrich
/// job of the episode). Exits 0: nothing was found, and nothing failed.
pub fn waiting(
    episode: &str,
    rule_set: &str,
    group: Option<&str>,
    min: Option<Severity>,
    detail: &str,
    document_errors: &[String],
) -> Value {
    shell("waiting", episode, rule_set, group, min, detail, document_errors.to_vec())
}

/// A run whose work failed; the failure is its one error (exit 2).
pub fn failed(
    episode: &str,
    rule_set: &str,
    group: Option<&str>,
    min: Option<Severity>,
    error: &str,
    document_errors: &[String],
) -> Value {
    let mut errors = document_errors.to_vec();
    errors.push(error.to_string());
    shell("failed", episode, rule_set, group, min, error, errors)
}

/// A run stopped by rule documents that do not read or parse: each message is
/// an error (exit 2), and nothing was applied.
pub fn invalid(group: Option<&str>, min: Option<Severity>, errors: Vec<String>) -> Value {
    shell("invalid", "", "", group, min, "", errors)
}

/// The project declares no rules: said plainly, exit 0. When rule documents
/// exist but do not parse, the run is `invalid` and exits 2 with their errors.
pub fn no_rules(group: Option<&str>, min: Option<Severity>, document_errors: &[String]) -> Value {
    let state = if document_errors.is_empty() { "no_rules" } else { "invalid" };
    shell(state, "", "", group, min, "", document_errors.to_vec())
}

fn shell(
    state: &str,
    episode: &str,
    rule_set: &str,
    group: Option<&str>,
    min: Option<Severity>,
    detail: &str,
    errors: Vec<String>,
) -> Value {
    let exit = exit_code(errors.len(), &[], min);
    json!({
        "state": state,
        "detail": (!detail.is_empty()).then_some(detail),
        "episode": episode,
        "rule_set": rule_set,
        "group": group,
        "min_severity": min.map(Severity::as_str),
        "concepts": [],
        "constraints": [],
        "skipped": [],
        "violations": [],
        "counts": {
            "concepts": 0, "failed": 0, "skipped": 0, "violations": 0,
            "by_severity": by_severity(std::iter::empty()),
        },
        "errors": errors,
        "exit": exit,
    })
}

fn violation_json(violation: &Violation) -> Value {
    json!({
        "rule": violation.rule,
        "severity": violation.severity.as_str(),
        "path": violation.path,
        "line": violation.line,
        "evidence": violation.evidence(),
    })
}

/// Every severity of the ladder, highest first, with its count (zero included).
fn by_severity(levels: impl Iterator<Item = Severity>) -> Map<String, Value> {
    let levels: Vec<Severity> = levels.collect();
    [
        Severity::Blocker,
        Severity::Critical,
        Severity::Major,
        Severity::Minor,
        Severity::Info,
    ]
    .into_iter()
    .map(|level| {
        let count = levels.iter().filter(|&&found| found == level).count();
        (level.as_str().to_string(), json!(count))
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch_rules::plan::{AppliedRule, Constraints, ConstraintResult, FailedRule, Plan};

    fn violation(rule: &str, severity: Severity, path: &str, line: i64) -> Violation {
        Violation {
            rule: rule.to_string(),
            severity,
            path: Some(path.to_string()),
            line: Some(line),
        }
    }

    /// An enrich result with two groups: `arch:default` applies a concept and
    /// evaluates two constraints; `arch:strict` selects the same concept and
    /// one constraint, and holds a plan of its own.
    fn job() -> EnrichReport {
        let severities = |entries: &[(&str, Severity)]| {
            entries
                .iter()
                .map(|(id, severity)| (id.to_string(), *severity))
                .collect()
        };
        EnrichReport {
            kind: "enrich".into(),
            episode: "ep-1".into(),
            rule_set: "rs".into(),
            applied: 1,
            failed: 0,
            skipped: 0,
            plans: vec![
                Plan {
                    group: "arch:default".into(),
                    error: None,
                    applied: vec![AppliedRule {
                        rule: "arch:Layer/Core".into(),
                        labels: 2,
                        relationships: 0,
                        properties: 1,
                        nodes: 1,
                    }],
                    failed: Vec::new(),
                    skipped: Vec::new(),
                    severities: severities(&[
                        ("arch:Layer/Core", Severity::Minor),
                        ("arch:NoCycle", Severity::Major),
                        ("arch:NoBlocker", Severity::Blocker),
                    ]),
                },
                Plan {
                    group: "arch:strict".into(),
                    error: None,
                    applied: Vec::new(),
                    failed: Vec::new(),
                    skipped: Vec::new(),
                    severities: severities(&[
                        ("arch:Layer/Core", Severity::Minor),
                        ("arch:NoCycle", Severity::Critical),
                    ]),
                },
            ],
            constraints: Some(Constraints {
                violations: 3,
                passing: 0,
                errors: 0,
                skipped: 0,
                results: vec![
                    ConstraintResult {
                        rule: "arch:NoCycle".into(),
                        status: Status::Violations,
                        severity: Some(Severity::Major),
                        violations: Some(2),
                        error: None,
                    },
                    ConstraintResult {
                        rule: "arch:NoBlocker".into(),
                        status: Status::Violations,
                        severity: Some(Severity::Blocker),
                        violations: Some(1),
                        error: None,
                    },
                ],
            }),
        }
    }

    fn rows() -> Vec<Violation> {
        vec![
            violation("arch:NoCycle", Severity::Major, "src/a.rs", 4),
            violation("arch:NoCycle", Severity::Major, "src/b.rs", 9),
            violation("arch:NoBlocker", Severity::Blocker, "src/c.rs", 1),
        ]
    }

    #[test]
    fn the_exit_status_gates_only_at_or_above_the_threshold() {
        let found = rows();
        // Nothing gates without a threshold, even a BLOCKER.
        assert_eq!(exit_code(0, &found, None), 0);
        // A MAJOR violation fails a MAJOR gate and passes a CRITICAL one.
        assert_eq!(exit_code(0, &found, Some(Severity::Major)), 1);
        assert_eq!(exit_code(0, &found[..2], Some(Severity::Critical)), 0);
        // No violations pass any gate.
        assert_eq!(exit_code(0, &[], Some(Severity::Info)), 0);
        // An error is exit 2 whatever the violations are.
        assert_eq!(exit_code(1, &found, Some(Severity::Major)), 2);
        assert_eq!(exit_code(1, &[], None), 2);
    }

    #[test]
    fn a_violation_below_the_threshold_passes_the_gate() {
        let minor = [violation("arch:Style", Severity::Minor, "src/x.rs", 2)];
        assert_eq!(exit_code(0, &minor, Some(Severity::Major)), 0);
        assert_eq!(exit_code(0, &minor, Some(Severity::Minor)), 1);
    }

    #[test]
    fn the_default_report_covers_every_group_once_and_sorts_by_severity() {
        let report = complete("ep-1", "rs", &job(), &rows(), None, Some(Severity::Major), &[]);
        assert_eq!(report["state"], "complete");
        assert_eq!(report["exit"], 1);
        assert_eq!(report["errors"], json!([]));
        // The concept runs once, though two groups select it.
        assert_eq!(report["concepts"].as_array().unwrap().len(), 1);
        let order: Vec<&str> = report["violations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["rule"].as_str().unwrap())
            .collect();
        assert_eq!(order, ["arch:NoBlocker", "arch:NoCycle", "arch:NoCycle"]);
    }

    #[test]
    fn a_group_restricts_the_report_to_its_own_members() {
        let report = complete("ep-1", "rs", &job(), &rows(), Some("arch:strict"), None, &[]);
        assert_eq!(report["group"], "arch:strict");
        assert_eq!(report["exit"], 0, "no gate without --min-severity");
        let rules: Vec<&str> = report["violations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["rule"].as_str().unwrap())
            .collect();
        assert_eq!(rules, ["arch:NoCycle", "arch:NoCycle"]);
        let constraints = report["constraints"].as_array().unwrap();
        assert_eq!(constraints.len(), 1);
        assert_eq!(constraints[0]["rule"], "arch:NoCycle");
    }

    #[test]
    fn an_undeclared_group_is_an_error() {
        let report = complete("ep-1", "rs", &job(), &rows(), Some("arch:nope"), None, &[]);
        assert_eq!(report["errors"], json!(["no rule group 'arch:nope'"]));
        assert_eq!(report["exit"], 2);
    }

    #[test]
    fn a_failed_rule_and_a_plan_error_are_errors() {
        let mut job = job();
        job.plans[0].failed = vec![FailedRule {
            rule: "arch:Layer/Core".into(),
            error: "boom".into(),
        }];
        job.plans[1].error =
            Some("rule 'arch:X' requires 'arch:Y', which matches no concept".into());
        let report = complete("ep-1", "rs", &job, &rows(), None, Some(Severity::Blocker), &[]);
        assert_eq!(report["exit"], 2);
        let errors: Vec<&str> = report["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e.as_str().unwrap())
            .collect();
        assert_eq!(
            errors,
            [
                "rule 'arch:Layer/Core' failed: boom",
                "group 'arch:strict': rule 'arch:X' requires 'arch:Y', which matches no concept",
            ]
        );
        assert_eq!(report["counts"]["failed"], 1);
    }

    #[test]
    fn a_waiting_or_empty_run_exits_zero_and_says_why() {
        let waiting = waiting("ep-2", "rs", None, Some(Severity::Major), "the enrich job is queued", &[]);
        assert_eq!(waiting["state"], "waiting");
        assert_eq!(waiting["detail"], "the enrich job is queued");
        assert_eq!(waiting["exit"], 0);
        let none = no_rules(None, Some(Severity::Major), &[]);
        assert_eq!(none["state"], "no_rules");
        assert_eq!(none["exit"], 0);
    }

    #[test]
    fn an_invalid_document_or_a_failed_job_exits_two() {
        let invalid = invalid(None, None, vec!["rule document docs/architecture/a.md: bad key".into()]);
        assert_eq!(invalid["state"], "invalid");
        assert_eq!(invalid["exit"], 2);
        let failed = failed("ep-3", "rs", None, None, "store is full", &[]);
        assert_eq!(failed["errors"], json!(["store is full"]));
        assert_eq!(failed["exit"], 2);
    }

    /// The `--format json` contract (`tests/fixtures/arch_rules/analyze_report.json`):
    /// the keys and values of a complete run with a threshold. CI reads these
    /// names, so the fixture is the frozen schema.
    #[test]
    fn the_json_report_matches_its_schema() {
        let found = [violation("arch:NoCycle", Severity::Major, "src/a.rs", 4)];
        let report = complete("ep-1", "rs", &job(), &found, Some("arch:default"), Some(Severity::Major), &[]);
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/arch_rules/analyze_report.json"
        ))
        .unwrap();
        assert_eq!(report, fixture);
    }

    #[test]
    fn a_malformed_document_is_reported_and_exits_two() {
        let document_errors = vec!["docs/architecture/a.md: rule 'arch:X' has no cypher".to_string()];
        let report = complete("ep-1", "rs", &job(), &[], None, Some(Severity::Blocker), &document_errors);
        assert_eq!(report["errors"], json!(document_errors));
        assert_eq!(report["exit"], 2);
        let none = no_rules(None, None, &document_errors);
        assert_eq!(none["state"], "invalid");
        assert_eq!(none["exit"], 2);
        let waiting = waiting("ep-2", "rs", None, None, "queued", &document_errors);
        assert_eq!(waiting["exit"], 2, "a malformed document errors even while the job waits");
    }

    #[test]
    fn violation_rows_read_back_from_the_store_keep_their_location() {
        let rows = vec![
            json!({"rule": "arch:A", "severity": "MINOR", "path": "src/a.rs", "line": 3}),
            json!({"rule": "arch:B", "severity": "CRITICAL", "path": null, "line": null}),
            json!({"rule": "arch:C", "severity": "UNKNOWN", "path": "x", "line": 1}),
        ];
        let found = violations(&rows);
        assert_eq!(found.len(), 2, "an unknown severity is dropped");
        assert_eq!(found[0].evidence(), "src/a.rs:3");
        assert_eq!(found[1].evidence(), "");
    }
}
