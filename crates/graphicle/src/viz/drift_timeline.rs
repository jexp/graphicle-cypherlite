//! The drift timeline (software-visualization-plan §5, D6 as data): churn,
//! co-change and violation counts bucketed by calendar month or quarter.
//!
//! The axis is the commits' `author_date`, in UTC: the history's own date, the
//! honest time axis of this store. (The writer keeps `commit_date` beside it;
//! this view does not read it.) Every bucket from the first to the last
//! placed point is in the series, empty ones included: a month with no
//! commits is a zero, a gap in drift, not an absent row.
//!
//! Per bucket:
//! - `commits`: the commits whose author date falls in it;
//! - `churn`: their `:Change` rows, the entries `git log --name-only` lists
//!   (a merge carries none);
//! - `co_change`: the file pairs those commits share, counted as the coupling
//!   derivation counts them (distinct files, two up to
//!   [`MAX_CHANGESET`](crate::git_history::coupling::MAX_CHANGESET), one pair
//!   per two). Summed over the series it is the whole-history `CO_CHANGED`
//!   total of the `all` window;
//! - `violations`: the `:Violation` rows of the episodes projected in it.
//!   A violation carries no commit time: it hangs off an episode, and an
//!   episode is stamped with its projection's wall clock (`created_at`). So
//!   this column sits on the episode axis, not the commit axis; the model
//!   names its axis in `violation_axis`, and violations of an episode with
//!   no readable stamp are counted as `unplaced_violations`, outside the
//!   series.
//!
//! A row's `evidence` is its first commit (earliest author date, then oid);
//! `first_commit` and `last_commit` carry the bucket's boundary commits. A
//! bucket without commits cites its label. The per-bucket detail lists in
//! the model (`top_commits` by churn, `top_violations` by rule) are cut to
//! `cap`; the series and its totals are never cut.
//!
//! `bucket` picks month or quarter. `focus` narrows the timeline to the
//! commits touching that file path, and to that path's violations; the
//! series keeps its full span so the narrowed gaps stay visible. `window` is
//! echoed in every answer but not read: the co-change totals here are derived
//! per commit, and the contract reads no window but the stored `CO_CHANGED`
//! keys.
//!
//! The supernode rule: every statement matches on `repository_id` and never
//! passes through `(:Project)`.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde_json::{Value, json};

use super::{Bucket, Params, TOP, View, run, text};
use crate::git_history::coupling::MAX_CHANGESET;
use crate::git_history::queries::Section;
use crate::store_graph::GraphStore;
use crate::{Error, Result};

/// The commits of the repository, with their author dates and subjects.
const COMMITS: &str = "MATCH (c:Commit {repository_id: $repo}) \
     RETURN c.oid AS oid, c.author_date AS at, c.subject AS subject";

/// Every change, with its commit: the per-commit file sets.
const CHANGES: &str = "MATCH (c:Commit {repository_id: $repo})-[:CONTAINS_CHANGE]->\
     (ch:Change {repository_id: $repo}) \
     RETURN c.oid AS oid, ch.path AS path";

/// Every violation the constraints wrote, with its episode and location.
const VIOLATIONS: &str = "MATCH (v:Violation {repository_id: $repo}) \
     RETURN v.episode AS episode, v.rule AS rule, v.severity AS severity, \
     v.path AS path, v.line AS line";

/// Each episode's projection stamp: the wall clock its graph was built at.
const EPISODES: &str = "MATCH (e:Episode {repository_id: $repo}) \
     RETURN e.episode AS episode, e.created_at AS at";

/// The commit axis, named in the model.
const AXIS: &str = "commit author_date (UTC)";
/// The violation axis, named in the model.
const VIOLATION_AXIS: &str = "episode created_at (wall clock: violations carry no commit time)";

/// One dated commit: its oid, author date, subject, and its churn and
/// co-change pair counts.
struct Commit {
    oid: String,
    at: DateTime<Utc>,
    subject: String,
    changes: i64,
    pairs: i64,
}

/// One violation placed on the episode axis.
struct Finding {
    at: DateTime<Utc>,
    episode: String,
    rule: String,
    severity: String,
    path: String,
    line: i64,
}

impl Finding {
    /// The row's citation: its `path:line`, or its rule when it names no path.
    fn evidence(&self) -> String {
        if self.path.is_empty() {
            self.rule.clone()
        } else {
            format!("{}:{}", self.path, self.line)
        }
    }
}

/// The drift timeline of `repository` at `params`.
pub fn view(graph: &GraphStore, repository: &str, params: &Params) -> Result<View> {
    let args = json!({"repo": repository});
    let mut touched: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in run(graph, CHANGES, &args)? {
        touched
            .entry(text(&row, "oid"))
            .or_default()
            .insert(text(&row, "path"));
    }

    let mut dated = Vec::new();
    let mut undated = 0_i64;
    for row in run(graph, COMMITS, &args)? {
        let oid = text(&row, "oid");
        let Some(at) = stamp(&row["at"]) else {
            undated += 1;
            continue;
        };
        let files = touched.get(&oid).map_or(0, BTreeSet::len);
        dated.push(Commit {
            changes: files as i64,
            pairs: pairs(files),
            subject: text(&row, "subject"),
            oid,
            at,
        });
    }
    dated.sort_by(|a, b| a.at.cmp(&b.at).then(a.oid.cmp(&b.oid)));

    let mut episodes: BTreeMap<String, DateTime<Utc>> = BTreeMap::new();
    for row in run(graph, EPISODES, &args)? {
        if let Some(at) = stamp(&row["at"]) {
            episodes.insert(text(&row, "episode"), at);
        }
    }
    let mut findings = Vec::new();
    let mut unplaced = 0_i64;
    for row in run(graph, VIOLATIONS, &args)? {
        let (episode, path) = (text(&row, "episode"), text(&row, "path"));
        if params.focus.as_ref().is_some_and(|focus| *focus != path) {
            continue;
        }
        let Some(&at) = episodes.get(&episode) else {
            unplaced += 1;
            continue;
        };
        findings.push(Finding {
            at,
            episode,
            rule: text(&row, "rule"),
            severity: text(&row, "severity"),
            path,
            line: row["line"].as_i64().unwrap_or(0),
        });
    }

    // The span covers every placed point, focus or not, so a narrowed
    // timeline keeps its gaps.
    let span: Vec<i64> = dated
        .iter()
        .map(|commit| index(commit.at, params.bucket))
        .chain(
            findings
                .iter()
                .map(|finding| index(finding.at, params.bucket)),
        )
        .collect();
    let (Some(&low), Some(&high)) = (span.iter().min(), span.iter().max()) else {
        return Ok(empty(params.bucket, undated, unplaced));
    };

    if let Some(focus) = &params.focus {
        dated.retain(|commit| {
            touched
                .get(&commit.oid)
                .is_some_and(|files| files.contains(focus))
        });
    }
    let mut commits_by: BTreeMap<i64, Vec<&Commit>> = BTreeMap::new();
    for commit in &dated {
        commits_by
            .entry(index(commit.at, params.bucket))
            .or_default()
            .push(commit);
    }
    let mut findings_by: BTreeMap<i64, Vec<&Finding>> = BTreeMap::new();
    for finding in &findings {
        findings_by
            .entry(index(finding.at, params.bucket))
            .or_default()
            .push(finding);
    }

    let mut rows = Vec::new();
    let mut series = Vec::new();
    let mut totals = Totals {
        unplaced,
        undated,
        ..Totals::default()
    };
    for idx in low..=high {
        let start = start_of(idx, params.bucket)?;
        let end = start_of(idx + 1, params.bucket)?;
        let label = label(start, params.bucket);
        let commits = commits_by.get(&idx).map_or(&[][..], Vec::as_slice);
        let found = findings_by.get(&idx).map_or(&[][..], Vec::as_slice);
        let first = commits.first().map(|commit| commit.oid.clone());
        let last = commits.last().map(|commit| commit.oid.clone());
        let churn: i64 = commits.iter().map(|commit| commit.changes).sum();
        let co_change: i64 = commits.iter().map(|commit| commit.pairs).sum();
        totals.commits += commits.len() as i64;
        totals.churn += churn;
        totals.co_change += co_change;
        totals.violations += found.len() as i64;

        let row = json!({
            "evidence": first.clone().unwrap_or_else(|| label.clone()),
            "bucket": label,
            "start": start.to_string(),
            "end": end.to_string(),
            "commits": commits.len(),
            "churn": churn,
            "co_change": co_change,
            "violations": found.len(),
            "first_commit": first,
            "last_commit": last,
        });
        let mut by_churn: Vec<&Commit> = commits.to_vec();
        by_churn.sort_by(|a, b| b.changes.cmp(&a.changes).then(a.oid.cmp(&b.oid)));
        let top_commits: Vec<Value> = by_churn
            .into_iter()
            .take(params.cap)
            .map(|commit| {
                json!({"evidence": commit.oid, "subject": commit.subject,
                    "at": commit.at.to_rfc3339(), "changes": commit.changes,
                    "pairs": commit.pairs})
            })
            .collect();
        let mut by_rule: Vec<&Finding> = found.to_vec();
        by_rule.sort_by(|a, b| {
            a.rule
                .cmp(&b.rule)
                .then(a.evidence().cmp(&b.evidence()))
                .then(a.episode.cmp(&b.episode))
        });
        let top_violations: Vec<Value> = by_rule
            .into_iter()
            .take(params.cap)
            .map(|finding| {
                json!({"evidence": finding.evidence(), "rule": finding.rule,
                    "severity": finding.severity, "episode": finding.episode})
            })
            .collect();
        let mut detail = row.clone();
        detail["top_commits"] = json!(top_commits);
        detail["top_violations"] = json!(top_violations);
        series.push(detail);
        rows.push(row);
    }

    let model = json!({
        "bucket": params.bucket.name(),
        "axis": AXIS,
        "violation_axis": VIOLATION_AXIS,
        "series": series,
        "totals": totals.json(),
        "cap": params.cap,
    });
    let section: Section = (title(params.bucket), rows);
    Ok(View {
        sections: vec![section],
        model,
        mermaid: None,
    })
}

/// The answer of a history with no placed point: no buckets, the totals of
/// what could not be placed.
fn empty(bucket: Bucket, undated: i64, unplaced: i64) -> View {
    let totals = Totals {
        undated,
        unplaced,
        ..Totals::default()
    };
    let section: Section = (title(bucket), Vec::new());
    View {
        sections: vec![section],
        model: json!({"bucket": bucket.name(), "axis": AXIS,
            "violation_axis": VIOLATION_AXIS, "series": [], "totals": totals.json(),
            "cap": TOP}),
        mermaid: None,
    }
}

/// The text report's section title for a bucket.
fn title(bucket: Bucket) -> &'static str {
    match bucket {
        Bucket::Month => "drift by month (commit author_date, UTC)",
        Bucket::Quarter => "drift by quarter (commit author_date, UTC)",
    }
}

/// The series' totals: what the buckets sum to, and what sits outside them.
#[derive(Default)]
struct Totals {
    commits: i64,
    churn: i64,
    co_change: i64,
    violations: i64,
    undated: i64,
    unplaced: i64,
}

impl Totals {
    fn json(&self) -> Value {
        json!({"commits": self.commits, "churn": self.churn,
            "co_change": self.co_change, "violations": self.violations,
            "undated_commits": self.undated, "unplaced_violations": self.unplaced})
    }
}

/// The file pairs `files` distinct files make, as the coupling derivation
/// counts them: none outside two to the changeset cap.
fn pairs(files: usize) -> i64 {
    if (2..=MAX_CHANGESET).contains(&files) {
        (files * (files - 1) / 2) as i64
    } else {
        0
    }
}

/// The bucket index of a time: months since year zero, or quarters.
fn index(at: DateTime<Utc>, bucket: Bucket) -> i64 {
    let (year, month0) = (i64::from(at.year()), i64::from(at.month0()));
    match bucket {
        Bucket::Month => year * 12 + month0,
        Bucket::Quarter => year * 4 + month0 / 3,
    }
}

/// The first day of the bucket `index` names.
fn start_of(index: i64, bucket: Bucket) -> Result<NaiveDate> {
    let (year, month) = match bucket {
        Bucket::Month => (index.div_euclid(12), index.rem_euclid(12) + 1),
        Bucket::Quarter => (index.div_euclid(4), index.rem_euclid(4) * 3 + 1),
    };
    i32::try_from(year)
        .ok()
        .and_then(|year| NaiveDate::from_ymd_opt(year, month as u32, 1))
        .ok_or_else(|| Error::bad("the history spans a bucket outside the calendar"))
}

/// The bucket's label: `2026-03`, or `2026-Q1`.
fn label(start: NaiveDate, bucket: Bucket) -> String {
    match bucket {
        Bucket::Month => format!("{:04}-{:02}", start.year(), start.month()),
        Bucket::Quarter => format!("{:04}-Q{}", start.year(), (start.month() - 1) / 3 + 1),
    }
}

/// A stored timestamp, read as RFC 3339 in UTC; `None` when it does not parse.
fn stamp(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}
