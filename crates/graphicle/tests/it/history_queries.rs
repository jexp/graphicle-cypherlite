//! The history metric pack (git-to-graph plan §6) over the real pipeline: a
//! scripted git history is synced (history, correlation, change resolution,
//! co-change) and each family's answer is checked against git's own output
//! for the same history. The ignored dogfood run does the same over a
//! checkout named by `GRAPHICLE_DOGFOOD_ROOT`, with its history and code
//! graph (`cargo test -- --ignored history_queries_dogfood --nocapture`).
use std::{collections::HashMap, path::Path, process::Command};

use graphicle_runtime::{
    git_history::queries::{self, Section},
    local,
    store::{Store, s},
};
use serde_json::Value;
use tempfile::TempDir;

use crate::arch::Fixture;

/// A commit's author: name and email.
const ADA: (&str, &str) = ("Ada Lovelace", "ada@example.com");
const BOB: (&str, &str) = ("Bob Barker", "bob@example.com");

/// `git` in `root`, with a fixed identity (the author's) and dates when given.
fn git(root: &Path, who: Option<(&str, &str)>, date: Option<&str>, args: &[&str]) -> String {
    let (name, email) = who.unwrap_or(ADA);
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_AUTHOR_NAME", name)
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_COMMITTER_NAME", name)
        .env("GIT_COMMITTER_EMAIL", email);
    if let Some(date) = date {
        command
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date);
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// Git's own churn: the commits that name each path in `git log --name-only`
/// over the whole history, the paths relative to the checkout (`--relative`),
/// as the reader reads them (the pathspec `.`).
fn git_churn(root: &Path) -> HashMap<String, i64> {
    let mut counts = HashMap::new();
    for path in git(
        root,
        None,
        None,
        &["log", "--relative", "--name-only", "--format=", "--", "."],
    )
    .lines()
    {
        if !path.is_empty() {
            *counts.entry(path.to_string()).or_insert(0) += 1;
        }
    }
    counts
}

/// Commit the working tree as `who`, dated `date`.
fn commit(fixture: &Fixture, who: (&str, &str), date: &str, subject: &str) {
    git(&fixture.root, Some(who), None, &["add", "-A"]);
    git(
        &fixture.root,
        Some(who),
        Some(date),
        &["commit", "-q", "-m", subject],
    );
}

/// Rewrite `files` (under `src/`) so each differs from the last commit, the
/// revision number `rev` in their text.
fn edit(fixture: &Fixture, files: &[&str], rev: u32) {
    for file in files {
        let body = match *file {
            "a" => "use crate::b::helper;\n\npub fn run() -> u32 {\n    helper()\n}\n".to_string(),
            "b" => "pub fn helper() -> u32 {\n    1\n}\n".to_string(),
            _ => "pub fn lone() -> u32 {\n    2\n}\n".to_string(),
        };
        fixture.write(&format!("src/{file}.rs"), &format!("{body}// rev {rev}\n"));
    }
}

/// The rows of one section of a family's report.
fn section<'a>(sections: &'a [Section], title: &str) -> &'a [Value] {
    sections
        .iter()
        .find(|(name, _)| *name == title)
        .map(|(_, rows)| rows.as_slice())
        .unwrap_or_else(|| panic!("no section {title:?} in {sections:?}"))
}

/// The row whose `evidence` is `evidence`.
fn row<'a>(rows: &'a [Value], evidence: &str) -> &'a Value {
    rows.iter()
        .find(|row| row["evidence"] == evidence)
        .unwrap_or_else(|| panic!("no row {evidence:?} in {rows:#?}"))
}

#[test]
fn the_pack_answers_a_scripted_history_as_git_does() {
    let mut fixture = Fixture::new();
    git(&fixture.root, None, None, &["init", "-q"]);
    // Six commits. a and b change together in 1-4, a and c in 1, 3, 5, 6;
    // ada touches a and c three times each and goes quiet in 2019, bob
    // keeps committing in 2026 (the newest commit).
    fixture.write("src/lib.rs", "mod a;\nmod b;\nmod c;\n");
    edit(&fixture, &["a", "b", "c"], 1);
    commit(&fixture, ADA, "2019-01-10T12:00:00Z", "first");
    edit(&fixture, &["a", "b"], 2);
    commit(&fixture, BOB, "2019-02-10T12:00:00Z", "tune helper");
    edit(&fixture, &["a", "b", "c"], 3);
    commit(&fixture, ADA, "2019-03-10T12:00:00Z", "wide edit");
    edit(&fixture, &["a", "b"], 4);
    commit(&fixture, BOB, "2019-04-10T12:00:00Z", "helper again");
    edit(&fixture, &["a", "c"], 5);
    commit(&fixture, ADA, "2019-05-10T12:00:00Z", "run and lone");
    edit(&fixture, &["a", "c"], 6);
    commit(&fixture, BOB, "2026-01-10T12:00:00Z", "modern edit");
    fixture.sync_and_build();

    let (repo, graph) = (&fixture.repo, fixture.graph());
    let truth = git_churn(&fixture.root);

    // The commit total is git's, for the same pathspec the reader walks.
    let commits = git(
        &fixture.root,
        None,
        None,
        &["rev-list", "--count", "HEAD", "--", "."],
    );
    assert_eq!(
        fixture.count("MATCH (c:Commit {repository_id: $repo}) RETURN count(c) AS n"),
        commits.parse::<i64>().unwrap(),
    );
    assert_eq!(commits, "6");

    // Hotspots: the file churn is git's name-only count per path.
    let hotspots = queries::sections(&graph, repo, "hotspots").unwrap();
    let files = section(&hotspots, "files by churn");
    assert_eq!(files.len(), truth.len(), "{files:#?}");
    for file in files {
        let path = file["file"].as_str().unwrap();
        assert_eq!(file["changes"].as_i64(), truth.get(path).copied(), "{path}");
    }
    assert_eq!(row(files, "src/a.rs")["changes"], 6);

    // Ownership: modifications only, so a.rs (ada's 2 against bob's 3) is
    // bob's, and c.rs (ada's 2 against bob's 1) is ada's.
    let ownership = queries::sections(&graph, repo, "ownership").unwrap();
    let owned = section(&ownership, "sole owners and dominant authors");
    let a = row(owned, "src/a.rs");
    assert_eq!(a["owner_email"], "bob@example.com");
    assert_eq!(
        (a["touches"].as_i64(), a["total"].as_i64()),
        (Some(3), Some(5))
    );
    assert_eq!(a["sole_owner"], true);
    assert_eq!(row(owned, "src/c.rs")["owner_email"], "ada@example.com");

    // Knowledge loss: ada left in 2019, a year before the newest commit,
    // with three touches on a.rs and on c.rs and two on b.rs.
    let loss = queries::sections(&graph, repo, "knowledge-loss").unwrap();
    let lost = section(&loss, "departed authors with churn");
    let mut files: Vec<&str> = lost
        .iter()
        .map(|row| row["file"].as_str().unwrap())
        .collect();
    files.sort();
    assert_eq!(files, ["src/a.rs", "src/c.rs"], "{lost:#?}");
    assert!(lost.iter().all(|row| row["email"] == "ada@example.com"));

    // Co-change: a and b share four commits, a and c four, b and c two
    // (below the threshold). Confidence and Jaccard follow the ported
    // formulas over the file churns of 6 (a), 4 (b) and 4 (c).
    let co = queries::sections(&graph, repo, "co-change").unwrap();
    let pairs = section(&co, "file pairs");
    let mut found: Vec<(String, String)> = pairs
        .iter()
        .map(|row| {
            (
                row["evidence"].as_str().unwrap().into(),
                row["with"].as_str().unwrap().into(),
            )
        })
        .collect();
    found.sort();
    assert_eq!(
        found,
        [
            ("src/a.rs".into(), "src/b.rs".into()),
            ("src/a.rs".into(), "src/c.rs".into())
        ],
        "{pairs:#?}"
    );
    let ab = row(pairs, "src/a.rs");
    assert_eq!(ab["count"], 4);
    assert!((ab["confidence"].as_f64().unwrap() - 4.0 / 4.0).abs() < 1e-9);
    assert!((ab["jaccard"].as_f64().unwrap() - 4.0 / 6.0).abs() < 1e-9);

    // Temporal coupling: a calls b's helper (a structural edge), while a and
    // c co-change with no call or import between them: the hidden coupling
    // comes first.
    let temporal = queries::sections(&graph, repo, "temporal-coupling").unwrap();
    let joined = section(
        &temporal,
        "co-changed file pairs, linked by calls or imports or not",
    );
    assert_eq!(joined[0]["with"], "src/c.rs", "{joined:#?}");
    assert_eq!(joined[0]["structural"], false);
    let linked = joined
        .iter()
        .find(|row| row["evidence"] == "src/a.rs" && row["with"] == "src/b.rs")
        .unwrap_or_else(|| panic!("no a-b pair in {joined:#?}"));
    assert_eq!(linked["structural"], true, "{linked:#?}");

    // The renderer names every section and cites a row's evidence first.
    let text = queries::render(&graph, repo, "ownership").unwrap();
    assert!(text.contains("  src/a.rs  "), "{text}");
}

/// Sync `root` (a checkout) and build its graph, the whole pipeline.
fn synced(root: &Path) -> (TempDir, Store, String) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path().join("store")).unwrap();
    let mut session = local::session(&store, true).unwrap();
    let synced = local::sync(&store, &mut session, root, None).unwrap();
    let repo = s(&synced, "repository_id").to_string();
    local::build_graph(&store, &session, &repo, false).unwrap();
    (dir, store, repo)
}

/// The dogfood run: this checkout's history and code graph. Ignored by
/// default (it syncs and projects the scope, minutes of work); run with
/// `GRAPHICLE_DOGFOOD_ROOT` set to a checkout root. Every comparison is
/// collected and the renders are printed before the assertion, so one run
/// shows the whole picture.
#[test]
#[ignore]
fn history_queries_dogfood() {
    let root = std::env::var("GRAPHICLE_DOGFOOD_ROOT").expect("GRAPHICLE_DOGFOOD_ROOT");
    let root = Path::new(&root);
    let (_dir, store, repo) = synced(root);
    let graph = (*store.graph()).clone();
    let mut mismatches = Vec::new();

    // Ingested commits carry a subject. A parent outside the pathspec's list
    // is written as a boundary node without one (the writer's parent link),
    // so it is not counted.
    let commits = git(
        root,
        None,
        None,
        &["rev-list", "--count", "HEAD", "--", "."],
    );
    let ingested = store_rows(
        &graph,
        &repo,
        "MATCH (c:Commit {repository_id: $repo}) WHERE c.subject IS NOT NULL RETURN count(c) AS n",
    );
    let boundary = store_rows(
        &graph,
        &repo,
        "MATCH (c:Commit {repository_id: $repo}) WHERE c.subject IS NULL RETURN count(c) AS n",
    );
    println!(
        "commits: git rev-list -- . = {commits}, ingested = {ingested}, boundary = {boundary}"
    );
    if ingested.to_string() != commits {
        mismatches.push(format!("commits: git {commits}, store {ingested}"));
    }

    // The top-churn files match git's name-only counts.
    let truth = git_churn(root);
    let hotspots = queries::sections(&graph, &repo, "hotspots").unwrap();
    for file in section(&hotspots, "files by churn") {
        let path = file["file"].as_str().unwrap();
        if file["changes"].as_i64() != truth.get(path).copied() {
            mismatches.push(format!(
                "churn {path}: store {}, git {:?}",
                file["changes"],
                truth.get(path)
            ));
        }
    }
    for family in [
        "hotspots",
        "ownership",
        "knowledge-loss",
        "co-change",
        "temporal-coupling",
    ] {
        println!(
            "== {family}\n{}",
            queries::render(&graph, &repo, family).unwrap()
        );
    }
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

/// The count a one-column `n` Cypher statement returns, for the dogfood run.
fn store_rows(graph: &graphicle_runtime::store_graph::GraphStore, repo: &str, cypher: &str) -> i64 {
    graph
        .read(|db| {
            graphicle_runtime::store_graph::rows(
                db,
                cypher,
                graphicle_runtime::store_graph::params(&serde_json::json!({"repo": repo})),
            )
        })
        .unwrap()[0]["n"]
        .as_i64()
        .unwrap()
}
