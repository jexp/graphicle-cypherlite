//! The typed view of one enrich run's result, defined once. The executor
//! builds it ([`EnrichReport`]) and serializes it as the `enrich` job's
//! result; the constraint evaluator and the analyze report read it back
//! through these structs, never through string keys on the JSON. The three
//! used to agree on the shape only by convention — a change made missing
//! keys read as null, rules became `not_selected` and violations vanished.
//! Now a change fails to (de)serialize instead.
//!
//! [`EnrichReport::to_value`] is the job-result JSON the store keeps;
//! [`EnrichReport::from_value`] reads it back. The tests pin the serialized
//! shape, so the executor, the evaluator and the report cannot drift apart.
use super::model::Severity;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// One concept's application, as its plan reports it: the writes the run's
/// provenance recorded for the rule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppliedRule {
    pub rule: String,
    pub labels: usize,
    pub relationships: usize,
    pub properties: usize,
    pub nodes: usize,
}

/// One rule the plan could not apply, with the error that stopped it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FailedRule {
    pub rule: String,
    pub error: String,
}

/// One group's plan within a run: the concepts it applied, the rules that
/// failed, the rules a failure skipped, and the effective severity of every
/// member — constraints included, whose severities the later evaluator reads
/// from here. `error` is the resolution error that stopped the plan before
/// any rule ran.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub applied: Vec<AppliedRule>,
    pub failed: Vec<FailedRule>,
    pub skipped: Vec<String>,
    pub severities: BTreeMap<String, Severity>,
}

/// What one constraint's evaluation found. The serialized names
/// (`"passing"`, `"violations"`, `"error"`, `"skipped"`, `"not_selected"`)
/// are the report's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Passing,
    Violations,
    Error,
    Skipped,
    NotSelected,
}

/// One constraint's evaluation: its status, and — for the statuses that ran
/// its query — its effective severity, its violation count or its error.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConstraintResult {
    pub rule: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub violations: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The constraint-evaluation half of a run's result: one entry per
/// constraint, in document order, with the run's counts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Constraints {
    pub violations: usize,
    pub passing: usize,
    pub errors: usize,
    pub skipped: usize,
    pub results: Vec<ConstraintResult>,
}

/// One enrich run's result: the plans the executor ran over the episode,
/// with the constraint evaluation the same job ran after them (`None` until
/// then — the executor's half alone).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnrichReport {
    pub kind: String,
    pub episode: String,
    pub rule_set: String,
    pub plans: Vec<Plan>,
    pub applied: usize,
    pub failed: usize,
    pub skipped: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<Constraints>,
}

impl EnrichReport {
    /// The run's job-result JSON: the shape the store keeps and `analyze`
    /// reads back.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_default()
    }

    /// The run's result read back from its job-result JSON.
    pub fn from_value(value: Value) -> std::result::Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
}

impl Constraints {
    /// The constraint half's JSON, as it sits inside the run's result.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A run's result with both halves and every status, as the enrich job
    /// leaves it.
    fn report() -> EnrichReport {
        let plan = |group: &str, severities: &[(&str, Severity)]| Plan {
            group: group.into(),
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
            severities: severities
                .iter()
                .map(|(id, severity)| (id.to_string(), *severity))
                .collect(),
        };
        EnrichReport {
            kind: "enrich".into(),
            episode: "ep-1".into(),
            rule_set: "rs".into(),
            applied: 1,
            failed: 0,
            skipped: 0,
            plans: vec![
                plan(
                    "arch:default",
                    &[
                        ("arch:Layer/Core", Severity::Minor),
                        ("arch:NoCycle", Severity::Major),
                        ("arch:NoBlocker", Severity::Blocker),
                    ],
                ),
                plan(
                    "arch:strict",
                    &[
                        ("arch:Layer/Core", Severity::Minor),
                        ("arch:NoCycle", Severity::Critical),
                    ],
                ),
            ],
            constraints: Some(Constraints {
                violations: 3,
                passing: 1,
                errors: 1,
                skipped: 1,
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
                    ConstraintResult {
                        rule: "arch:Quiet".into(),
                        status: Status::Passing,
                        severity: Some(Severity::Minor),
                        violations: Some(0),
                        error: None,
                    },
                    ConstraintResult {
                        rule: "arch:Broken".into(),
                        status: Status::Error,
                        severity: Some(Severity::Major),
                        violations: None,
                        error: Some("query failed".into()),
                    },
                    ConstraintResult {
                        rule: "arch:After".into(),
                        status: Status::Skipped,
                        severity: None,
                        violations: None,
                        error: None,
                    },
                    ConstraintResult {
                        rule: "arch:Elsewhere".into(),
                        status: Status::NotSelected,
                        severity: None,
                        violations: None,
                        error: None,
                    },
                ],
            }),
        }
    }

    #[test]
    fn the_job_result_json_keeps_its_shape() {
        // The keys the executor, the evaluator and the report agree on, as
        // one pinned value: severities as their ladder names, statuses as
        // the report's words, and only the fields each status carries.
        assert_eq!(
            report().to_value(),
            json!({
                "kind": "enrich", "episode": "ep-1", "rule_set": "rs",
                "applied": 1, "failed": 0, "skipped": 0,
                "plans": [
                    {"group": "arch:default",
                     "applied": [{"rule": "arch:Layer/Core", "labels": 2, "relationships": 0,
                                  "properties": 1, "nodes": 1}],
                     "failed": [], "skipped": [],
                     "severities": {"arch:Layer/Core": "MINOR", "arch:NoCycle": "MAJOR",
                                    "arch:NoBlocker": "BLOCKER"}},
                    {"group": "arch:strict",
                     "applied": [{"rule": "arch:Layer/Core", "labels": 2, "relationships": 0,
                                  "properties": 1, "nodes": 1}],
                     "failed": [], "skipped": [],
                     "severities": {"arch:Layer/Core": "MINOR", "arch:NoCycle": "CRITICAL"}},
                ],
                "constraints": {
                    "violations": 3, "passing": 1, "errors": 1, "skipped": 1,
                    "results": [
                        {"rule": "arch:NoCycle", "status": "violations", "severity": "MAJOR",
                         "violations": 2},
                        {"rule": "arch:NoBlocker", "status": "violations", "severity": "BLOCKER",
                         "violations": 1},
                        {"rule": "arch:Quiet", "status": "passing", "severity": "MINOR",
                         "violations": 0},
                        {"rule": "arch:Broken", "status": "error", "severity": "MAJOR",
                         "error": "query failed"},
                        {"rule": "arch:After", "status": "skipped"},
                        {"rule": "arch:Elsewhere", "status": "not_selected"},
                    ],
                },
            })
        );
    }

    #[test]
    fn the_report_round_trips_through_its_job_result_json() {
        let report = report();
        assert_eq!(EnrichReport::from_value(report.to_value()).unwrap(), report);
    }

    #[test]
    fn a_plan_error_and_an_absent_constraint_half_keep_their_shape() {
        let mut report = report();
        // The plan as an unresolved group leaves it: the error, and the
        // empty halves every plan carries.
        report.plans[0].error = Some("rule cycle through 'arch:A'".into());
        report.plans[0].applied = Vec::new();
        report.plans[0].severities = BTreeMap::new();
        report.constraints = None;
        let value = report.to_value();
        assert_eq!(
            value["plans"][0]["error"],
            json!("rule cycle through 'arch:A'")
        );
        assert_eq!(value["plans"][0]["applied"], json!([]));
        assert_eq!(value["plans"][0]["severities"], json!({}));
        // The executor's half alone serializes without the constraints key.
        assert!(value.get("constraints").is_none());
        assert_eq!(EnrichReport::from_value(value).unwrap(), report);
    }
}
