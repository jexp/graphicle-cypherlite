//! The history metric pack (git-to-graph plan §6): five families read from
//! the anchored history model and served behind
//! `graphicle-graph://history/<family>` resources ([`crate::graph_tools`]).
//!
//! | family              | ported from (research/jqa-git-queries.md)              |
//! |---------------------|---------------------------------------------------------|
//! | `hotspots`          | 4 (file churn); symbol degree x span churn is new      |
//! | `ownership`         | 10 (ownership and the bus-factor read)                 |
//! | `knowledge-loss`    | 12 (departed contributors with churn)                  |
//! | `co-change`         | 7 (pair support, confidence, lift, Jaccard), file and symbol |
//! | `temporal-coupling` | 7 joined to the code graph's `calls` and `imports`     |
//!
//! Each family is a few Cypher statements plus the arithmetic the ported
//! queries did in Cypher and CypherLite cannot (percentiles, `duration`,
//! APOC pairing). Every statement anchors on `repository_id` and never
//! passes through `(:Project)` (the supernode rule). `CO_CHANGED` is stored
//! canonically (the smaller endpoint is the source), so the pair reads are
//! undirected and keep one orientation with `a < b`.
//!
//! # Evidence
//!
//! Every row names what it is about in `evidence`: a symbol's
//! `relpath:line`, where `line` is the first line a span-basis `TOUCHES`
//! edge recorded for it, or a file's path (the model has no file line).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, FixedOffset};
use serde_json::{Value, json};

use crate::store_graph::{self, GraphStore};
use crate::{Error, Result};

/// The resource URI prefix of the pack: `graphicle-graph://history/<family>`.
pub const PREFIX: &str = "graphicle-graph://history/";
/// The coupling window the pair families read: the whole-history derivation.
pub const WINDOW: &str = "all";
/// Rows one section reports.
pub const TOP: usize = 25;
/// Days between an author's last commit and the newest commit of the
/// history after which the author counts as departed (ported 12).
pub const DEPARTED_DAYS: i64 = 365;
/// Touches an author needs on one file before the file's knowledge is lost
/// with them (ported 12).
pub const LOST_TOUCHES: i64 = 3;
/// The share of a file's touches one author needs to be its sole owner
/// (ported 10's bus-factor read: `ownership > 0.5`).
pub const SOLE_OWNER: f64 = 0.5;

/// One named section of a family's report: a title and its rows.
pub type Section = (&'static str, Vec<Value>);

/// The families: resource slug, name and description.
pub const FAMILIES: [(&str, &str, &str); 5] = [
    (
        "hotspots",
        "History Hotspots",
        "Symbol degree times churn, and the most-changed files",
    ),
    (
        "ownership",
        "File Ownership",
        "Sole-owner files and their dominant author",
    ),
    (
        "knowledge-loss",
        "Knowledge Loss",
        "Departed authors with churn on files",
    ),
    (
        "co-change",
        "Co-change",
        "File and symbol pairs that change together",
    ),
    (
        "temporal-coupling",
        "Temporal Coupling",
        "Co-changed file pairs against the code graph's calls and imports",
    ),
];

/// Changes per file (ported 4: change entries, one per commit and path).
const FILE_CHURN: &str = "MATCH (:Commit {repository_id: $repo})-[:CONTAINS_CHANGE]->\
     (ch:Change {repository_id: $repo})-[:MODIFIES]->(f:File {repository_id: $repo}) \
     RETURN f.path AS file, count(ch) AS changes";

/// Commits touching each symbol through a span `TOUCHES`, with the first
/// touched line (the evidence line).
const SYMBOL_CHURN: &str = "MATCH (ch:Change {repository_id: $repo})-[t:TOUCHES {basis: 'span'}]->\
     (s:Symbol {repository_id: $repo}) \
     RETURN s.stable_id AS symbol, s.relpath AS path, s.qualified_name AS name, \
     count(DISTINCT ch.commit) AS churn, min(t.line_start) AS line";

/// Live `CALLS` and `IMPORTS` edges incident to each symbol. The projection
/// writes the extractors' relation strings upper-cased, and a Rust `use` as
/// `IMPORTS_FROM`, so the import kinds are listed as the store holds them.
const SYMBOL_DEGREE: &str = "MATCH (s:Symbol {repository_id: $repo})-[r]-(:Symbol {repository_id: $repo}) \
     WHERE type(r) IN ['CALLS', 'IMPORTS', 'IMPORTS_FROM'] AND r.valid_from_episode IS NOT NULL \
     AND r.valid_to_episode IS NULL \
     RETURN s.stable_id AS symbol, count(r) AS degree";

/// Live `CALLS` and `IMPORTS` edges between files, by their symbols' paths.
pub(crate) const STRUCTURAL: &str = "MATCH (a:Symbol {repository_id: $repo})-[r]->(b:Symbol {repository_id: $repo}) \
     WHERE type(r) IN ['CALLS', 'IMPORTS', 'IMPORTS_FROM'] AND r.valid_from_episode IS NOT NULL \
     AND r.valid_to_episode IS NULL \
     RETURN DISTINCT a.relpath AS from, b.relpath AS to, type(r) AS relation";

/// Commits with at least one change (the global update count, ported 7).
const COMMIT_TOTAL: &str = "MATCH (c:Commit {repository_id: $repo})-[:CONTAINS_CHANGE]->\
     (:Change {repository_id: $repo}) RETURN count(DISTINCT c) AS n";

/// Commits with a span-basis symbol touch (the symbol-level update count).
const SPAN_COMMITS: &str = "MATCH (ch:Change {repository_id: $repo})-[:TOUCHES {basis: 'span'}]->\
     (:Symbol {repository_id: $repo}) RETURN count(DISTINCT ch.commit) AS n";

/// Modifications per (file, author) (ported 10: kind `M`, so renames and
/// additions do not inflate ownership).
const OWNERSHIP: &str = "MATCH (c:Commit {repository_id: $repo})-[:CONTAINS_CHANGE]->\
     (:Change {repository_id: $repo, kind: 'M'})-[:MODIFIES]->(f:File {repository_id: $repo}), \
     (a:Person {repository_id: $repo})-[:AUTHORED]->(c) \
     RETURN f.path AS file, a.email AS email, a.name AS name, count(*) AS touches";

/// Changes per (file, author) (ported 12).
const KNOWLEDGE: &str = "MATCH (c:Commit {repository_id: $repo})-[:CONTAINS_CHANGE]->\
     (:Change {repository_id: $repo})-[:MODIFIES]->(f:File {repository_id: $repo}), \
     (a:Person {repository_id: $repo})-[:AUTHORED]->(c) \
     RETURN f.path AS file, a.email AS email, a.name AS name, count(*) AS touches";

/// Each author's last commit (ported 12's departure test, against the
/// history's own newest commit rather than the wall clock).
const AUTHOR_LAST: &str = "MATCH (a:Person {repository_id: $repo})-[:AUTHORED]->\
     (c:Commit {repository_id: $repo}) \
     RETURN a.email AS email, max(c.author_date) AS last";

/// The stored file co-change pairs of one window, undirected, each once
/// (`a < b`), above the reporting threshold (ported 7: more than two).
const CO_CHANGED_FILES: &str = "MATCH (a:File {repository_id: $repo})-[r:CO_CHANGED {window: $window}]-\
     (b:File {repository_id: $repo}) \
     WHERE a.path < b.path AND r.count > 2 \
     RETURN a.path AS a, b.path AS b, r.count AS n";

/// The stored symbol co-change pairs of one window, as [`CO_CHANGED_FILES`].
const CO_CHANGED_SYMBOLS: &str = "MATCH (a:Symbol {repository_id: $repo})-[r:CO_CHANGED {window: $window}]-\
     (b:Symbol {repository_id: $repo}) \
     WHERE a.stable_id < b.stable_id AND r.count > 2 \
     RETURN a.stable_id AS a, b.stable_id AS b, r.count AS n";

/// The pack's family for a resource URI, its slug.
pub fn family(uri: &str) -> Option<&'static str> {
    FAMILIES
        .iter()
        .map(|(slug, _, _)| *slug)
        .find(|slug| uri.strip_prefix(PREFIX) == Some(slug))
}

/// The pack's resource specs, in the shape `resources/list` publishes.
pub fn resources() -> Vec<Value> {
    FAMILIES
        .iter()
        .map(|(slug, name, description)| {
            json!({"uri": format!("{PREFIX}{slug}"), "name": name,
                "description": description, "mimeType": "text/plain"})
        })
        .collect()
}

/// The family's report as text: one block per section, one line per row,
/// the row's `evidence` first.
pub fn render(graph: &GraphStore, repository_id: &str, slug: &str) -> Result<String> {
    let mut out = Vec::new();
    for (title, rows) in sections(graph, repository_id, slug)? {
        out.push(format!("{title}:"));
        if rows.is_empty() {
            out.push("  none".to_string());
        }
        out.extend(rows.iter().map(|row| format!("  {}", line(row))));
    }
    Ok(out.join("\n"))
}

/// The family's sections, rows sorted and cut to [`TOP`] (the tests and the
/// renderer both read these).
pub fn sections(graph: &GraphStore, repository_id: &str, slug: &str) -> Result<Vec<Section>> {
    Ok(match slug {
        "hotspots" => hotspots(graph, repository_id)?,
        "ownership" => vec![(
            "sole owners and dominant authors",
            ownership(graph, repository_id)?,
        )],
        "knowledge-loss" => vec![(
            "departed authors with churn",
            knowledge_loss(graph, repository_id)?,
        )],
        "co-change" => co_change(graph, repository_id)?,
        "temporal-coupling" => vec![(
            "co-changed file pairs, linked by calls or imports or not",
            temporal(graph, repository_id)?,
        )],
        _ => return Err(Error::bad(format!("Unknown history family: {slug}"))),
    })
}

fn hotspots(graph: &GraphStore, repository_id: &str) -> Result<Vec<Section>> {
    let args = json!({"repo": repository_id});
    let degree: HashMap<String, i64> = run(graph, SYMBOL_DEGREE, &args)?
        .iter()
        .map(|row| (text(row, "symbol"), int(row, "degree")))
        .collect();
    let mut symbols: Vec<Value> = run(graph, SYMBOL_CHURN, &args)?
        .iter()
        .filter_map(|row| {
            let edges = *degree.get(&text(row, "symbol"))?;
            let churn = int(row, "churn");
            Some(json!({"evidence": span(row), "symbol": text(row, "name"),
                "churn": churn, "degree": edges, "score": churn * edges}))
        })
        .collect();
    symbols.sort_by(|a, b| {
        int(b, "score")
            .cmp(&int(a, "score"))
            .then(evidence(a).cmp(&evidence(b)))
    });
    symbols.truncate(TOP);

    let mut files: Vec<Value> = run(graph, FILE_CHURN, &args)?
        .iter()
        .map(|row| {
            json!({"evidence": text(row, "file"), "file": text(row, "file"),
            "changes": int(row, "changes")})
        })
        .collect();
    files.sort_by(|a, b| {
        int(b, "changes")
            .cmp(&int(a, "changes"))
            .then(evidence(a).cmp(&evidence(b)))
    });
    files.truncate(TOP);
    Ok(vec![
        ("symbols by degree times churn", symbols),
        ("files by churn", files),
    ])
}

/// One author's modifications of one file (the ownership read's row).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Author {
    pub email: String,
    pub name: String,
    pub touches: i64,
}

/// The dominant author of a file: the most touches wins; a tie goes to the
/// smaller email, so the answer is stable across runs.
pub(crate) fn dominant(authors: &[Author]) -> Option<&Author> {
    authors
        .iter()
        .max_by(|a, b| a.touches.cmp(&b.touches).then(b.email.cmp(&a.email)))
}

/// Each file's authors with their modification counts, by path (ported 10:
/// kind `M`, so renames and additions do not inflate ownership). The
/// ownership family and the knowledge-map view both read it.
pub(crate) fn file_authors(
    graph: &GraphStore,
    repository_id: &str,
) -> Result<BTreeMap<String, Vec<Author>>> {
    let mut per_file: BTreeMap<String, Vec<Author>> = BTreeMap::new();
    for row in run(graph, OWNERSHIP, &json!({"repo": repository_id}))? {
        per_file
            .entry(text(&row, "file"))
            .or_default()
            .push(Author {
                email: text(&row, "email"),
                name: text(&row, "name"),
                touches: int(&row, "touches"),
            });
    }
    Ok(per_file)
}

fn ownership(graph: &GraphStore, repository_id: &str) -> Result<Vec<Value>> {
    let mut rows: Vec<Value> = file_authors(graph, repository_id)?
        .into_iter()
        .filter_map(|(file, authors)| {
            let total: i64 = authors.iter().map(|author| author.touches).sum();
            let top = dominant(&authors)?;
            let ownership = top.touches as f64 / total as f64;
            Some(json!({"evidence": file, "file": file, "owner": top.name,
                "owner_email": top.email, "ownership": ownership,
                "touches": top.touches, "total": total,
                "contributors": authors.len(), "sole_owner": ownership > SOLE_OWNER}))
        })
        .collect();
    rows.sort_by(|a, b| {
        b["ownership"]
            .as_f64()
            .partial_cmp(&a["ownership"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(int(b, "total").cmp(&int(a, "total")))
            .then(evidence(a).cmp(&evidence(b)))
    });
    rows.truncate(TOP);
    Ok(rows)
}

fn knowledge_loss(graph: &GraphStore, repository_id: &str) -> Result<Vec<Value>> {
    let args = json!({"repo": repository_id});
    let lasts: Vec<(String, String, DateTime<FixedOffset>)> = run(graph, AUTHOR_LAST, &args)?
        .iter()
        .filter_map(|row| {
            let at = parse_date(&text(row, "last"))?;
            Some((text(row, "email"), text(row, "last"), at))
        })
        .collect();
    let Some(newest) = lasts.iter().map(|(_, _, at)| *at).max() else {
        return Ok(Vec::new());
    };
    let departed: HashMap<String, String> = lasts
        .into_iter()
        .filter(|(_, _, at)| (newest - *at).num_days() > DEPARTED_DAYS)
        .map(|(email, last, _)| (email, last))
        .collect();
    let mut rows: Vec<Value> = run(graph, KNOWLEDGE, &args)?
        .iter()
        .filter_map(|row| {
            let last = departed.get(&text(row, "email"))?;
            let touches = int(row, "touches");
            (touches >= LOST_TOUCHES).then(|| {
                json!({"evidence": text(row, "file"), "file": text(row, "file"),
                    "author": text(row, "name"), "email": text(row, "email"),
                    "touches": touches, "last_commit": last})
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        int(b, "touches")
            .cmp(&int(a, "touches"))
            .then(evidence(a).cmp(&evidence(b)))
    });
    rows.truncate(TOP);
    Ok(rows)
}

fn co_change(graph: &GraphStore, repository_id: &str) -> Result<Vec<Section>> {
    let args = json!({"repo": repository_id, "window": WINDOW});
    let files = file_churn(graph, repository_id)?;
    let file_total = total(graph, COMMIT_TOTAL, repository_id)?;
    let file_pairs = pairs(graph, CO_CHANGED_FILES, &args, file_total, &files)?;

    let symbols = symbol_churn(graph, repository_id)?;
    let symbol_total = total(graph, SPAN_COMMITS, repository_id)?;
    let symbol_pairs = pairs(graph, CO_CHANGED_SYMBOLS, &args, symbol_total, &symbols)?;
    Ok(vec![
        ("file pairs", file_pairs),
        ("symbol pairs", symbol_pairs),
    ])
}

fn temporal(graph: &GraphStore, repository_id: &str) -> Result<Vec<Value>> {
    let args = json!({"repo": repository_id, "window": WINDOW});
    let files = file_churn(graph, repository_id)?;
    let file_total = total(graph, COMMIT_TOTAL, repository_id)?;
    let mut linked: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for row in run(graph, STRUCTURAL, &json!({"repo": repository_id}))? {
        let (from, to) = (text(&row, "from"), text(&row, "to"));
        if from != to {
            linked
                .entry(ordered(from, to))
                .or_default()
                .insert(text(&row, "relation"));
        }
    }
    let mut rows: Vec<Value> = pairs(graph, CO_CHANGED_FILES, &args, file_total, &files)?
        .into_iter()
        .map(|mut row| {
            let key = ordered(text(&row, "evidence"), text(&row, "with"));
            let relations: Vec<String> = linked.get(&key).into_iter().flatten().cloned().collect();
            row["structural"] = json!(!relations.is_empty());
            row["relations"] = json!(if relations.is_empty() {
                "none".to_string()
            } else {
                relations.join(",")
            });
            row
        })
        .collect();
    // Hidden coupling first: the pairs that change together with no call or
    // import between them, then the linked pairs; each by its count.
    rows.sort_by(|a, b| {
        a["structural"]
            .as_bool()
            .cmp(&b["structural"].as_bool())
            .then(int(b, "count").cmp(&int(a, "count")))
    });
    rows.truncate(TOP);
    Ok(rows)
}

/// Co-change pairs of one level with the ported metrics: `confidence` is the
/// pair count over the rarer endpoint's changes (min-confidence), `support`
/// over all commits, `jaccard` over the commits either endpoint is in, and
/// `lift` against the independence expectation.
fn pairs(
    graph: &GraphStore,
    cypher: &str,
    args: &Value,
    total: i64,
    churn: &HashMap<String, (String, i64)>,
) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for row in run(graph, cypher, args)? {
        let (Some((left, a)), Some((right, b))) =
            (churn.get(&text(&row, "a")), churn.get(&text(&row, "b")))
        else {
            continue;
        };
        let count = int(&row, "n");
        let (c, a, b, g) = (count as f64, *a as f64, *b as f64, total as f64);
        rows.push(json!({"evidence": left, "with": right, "count": count,
            "confidence": c / a.min(b), "support": c / g,
            "jaccard": c / (a + b - c), "lift": c * g / (a * b)}));
    }
    rows.sort_by(|a, b| {
        int(b, "count")
            .cmp(&int(a, "count"))
            .then(evidence(a).cmp(&evidence(b)))
    });
    rows.truncate(TOP);
    Ok(rows)
}

/// The changes of each file, by path: `(path, changes)`.
pub(crate) fn file_churn(
    graph: &GraphStore,
    repository_id: &str,
) -> Result<HashMap<String, (String, i64)>> {
    Ok(run(graph, FILE_CHURN, &json!({"repo": repository_id}))?
        .iter()
        .map(|row| {
            let file = text(row, "file");
            (file.clone(), (file, int(row, "changes")))
        })
        .collect())
}

/// The span-touch commits of each symbol, by stable id: `(path:line, commits)`.
fn symbol_churn(graph: &GraphStore, repository_id: &str) -> Result<HashMap<String, (String, i64)>> {
    Ok(run(graph, SYMBOL_CHURN, &json!({"repo": repository_id}))?
        .iter()
        .map(|row| (text(row, "symbol"), (span(row), int(row, "churn"))))
        .collect())
}

/// One aggregate count of a statement's `n` column (zero when absent).
fn total(graph: &GraphStore, cypher: &str, repository_id: &str) -> Result<i64> {
    Ok(run(graph, cypher, &json!({"repo": repository_id}))?
        .first()
        .map(|row| int(row, "n"))
        .unwrap_or(0))
}

fn run(graph: &GraphStore, cypher: &str, args: &Value) -> Result<Vec<Value>> {
    graph.read(|db| store_graph::rows(db, cypher, store_graph::params(args)))
}

fn text(row: &Value, key: &str) -> String {
    store_graph::s(row, key).to_string()
}

fn int(row: &Value, key: &str) -> i64 {
    row[key].as_i64().unwrap_or(0)
}

/// A symbol's `path:line` evidence from a [`SYMBOL_CHURN`] row.
fn span(row: &Value) -> String {
    format!("{}:{}", text(row, "path"), int(row, "line"))
}

fn evidence(row: &Value) -> String {
    text(row, "evidence")
}

fn ordered(a: String, b: String) -> (String, String) {
    if a <= b { (a, b) } else { (b, a) }
}

fn parse_date(text: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(text).ok()
}

/// One report line: the evidence, then the row's other fields by name. The
/// viz pack prints its rows through the same line, so both packs report
/// identically.
pub(crate) fn line(row: &Value) -> String {
    let fields: Vec<String> = row
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| key.as_str() != "evidence")
        .map(|(key, value)| match value {
            Value::Number(n) if n.is_f64() => format!("{key}={:.2}", value.as_f64().unwrap_or(0.0)),
            Value::String(s) => format!("{key}={s}"),
            other => format!("{key}={other}"),
        })
        .collect();
    format!("{}  {}", evidence(row), fields.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_family_of_a_uri_is_its_slug_and_nothing_else() {
        assert_eq!(
            family("graphicle-graph://history/hotspots"),
            Some("hotspots")
        );
        assert_eq!(
            family("graphicle-graph://history/co-change"),
            Some("co-change")
        );
        assert_eq!(family("graphicle-graph://history/hotspots/x"), None);
        assert_eq!(family("graphicle-graph://stats"), None);
    }

    #[test]
    fn every_family_is_listed_as_a_resource_under_the_prefix() {
        let uris: Vec<String> = resources()
            .iter()
            .map(|spec| spec["uri"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(uris.len(), FAMILIES.len());
        assert!(uris.iter().all(|uri| family(uri).is_some()));
    }

    #[test]
    fn a_pair_is_stored_and_read_in_one_orientation() {
        assert_eq!(ordered("b".into(), "a".into()), ("a".into(), "b".into()));
        assert_eq!(ordered("a".into(), "b".into()), ("a".into(), "b".into()));
    }

    #[test]
    fn a_report_line_leads_with_its_evidence() {
        let row = json!({"evidence": "src/a.rs:12", "churn": 3, "score": 1.5});
        assert_eq!(line(&row), "src/a.rs:12  churn=3 score=1.50");
    }
}
