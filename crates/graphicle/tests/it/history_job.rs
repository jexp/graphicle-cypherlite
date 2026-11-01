//! The history job at the sync level (git-to-graph plan §4): a projection
//! that completes for a project whose folder is a git repository queues the
//! history ingestion behind it; a folder that is not a git repository
//! queues nothing, and the projection itself succeeds all the same.
use graphicle_runtime::{git_history, store::s};
use serde_json::{Value, json};

use crate::arch::Fixture;

/// The project's projection jobs, oldest first, their results decoded.
fn projections(fixture: &Fixture) -> Vec<Value> {
    fixture
        .query(
            "MATCH (j:Job {repository_id: $repo, kind: 'project'}) \
             RETURN j.state AS state, j.result AS result ORDER BY j.created_at",
            json!({}),
        )
        .into_iter()
        .map(|mut row| {
            row["result"] = serde_json::from_str::<Value>(s(&row, "result")).unwrap_or(Value::Null);
            row
        })
        .collect()
}

#[test]
fn a_sync_of_a_folder_that_is_not_a_git_repository_queues_no_history() {
    // The shared fixture's folder is a plain project directory in a tempdir
    // — no `.git`, the common case for every synced folder that is not a
    // checkout.
    let mut fixture = Fixture::new();
    fixture.sync_and_build();

    let projections = projections(&fixture);
    let latest = projections
        .last()
        .expect("the drain ran the projection job");
    assert_eq!(
        latest["state"],
        json!("complete"),
        "the projection itself succeeds: {latest:#}"
    );
    // The enqueue point reported the folder as out of scope — a skip, not
    // a failure — inside the projection's own result.
    let history = &latest["result"]["history"];
    assert_eq!(history["queued"], json!(false), "{history:#}");
    assert_eq!(history["reason"], json!("not a git repository"));
    // And nothing was queued: no history job exists for the project.
    assert_eq!(
        fixture.query(
            "MATCH (j:Job {repository_id: $repo, kind: $kind}) RETURN count(j) AS n",
            json!({"kind": git_history::KIND}),
        )[0]["n"],
        json!(0)
    );
}

/// `git` in the fixture checkout, with a fixed identity and no signing.
fn git(root: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_AUTHOR_NAME", "Ada Lovelace")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada Lovelace")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// Make the fixture folder a git checkout with its one file committed.
fn commit_fixture(fixture: &Fixture) -> String {
    git(&fixture.root, &["init", "-q"]);
    git(&fixture.root, &["add", "-A"]);
    git(&fixture.root, &["commit", "-q", "-m", "first"]);
    git(&fixture.root, &["rev-parse", "HEAD"])
}

/// The `(:Episode)-[:AT_COMMIT]->(:Commit)` edges of the project.
fn correlated(fixture: &Fixture) -> Vec<Value> {
    fixture.query(
        "MATCH (e:Episode {repository_id: $repo})-[:AT_COMMIT]->(c:Commit {repository_id: $repo}) \
         RETURN e.episode AS episode, c.oid AS oid",
        json!({}),
    )
}

#[test]
fn a_sync_at_head_with_a_clean_tree_correlates_the_episode_with_the_commit() {
    let mut fixture = Fixture::new();
    let head = commit_fixture(&fixture);
    fixture.sync_and_build();

    assert_eq!(
        fixture.count("MATCH (c:Commit {repository_id: $repo}) RETURN count(c) AS n"),
        1,
        "the history job ingested the commit"
    );
    let edges = correlated(&fixture);
    assert_eq!(edges.len(), 1, "{edges:#?}");
    assert_eq!(edges[0]["oid"], json!(head));
}

#[test]
fn a_dirty_tree_leaves_the_episode_a_working_tree_snapshot() {
    let mut fixture = Fixture::new();
    commit_fixture(&fixture);
    fixture.write("src/lib.rs", "pub fn run() { /* uncommitted */ }\n");
    fixture.sync_and_build();
    assert!(
        correlated(&fixture).is_empty(),
        "an uncommitted change keeps the episode off the commit"
    );

    // Restoring the committed content makes the head set the HEAD tree: the
    // same sync path now correlates.
    fixture.write("src/lib.rs", "pub fn run() {}\n");
    fixture.sync_and_build();
    assert_eq!(correlated(&fixture).len(), 1);
}

#[test]
fn resyncing_the_same_heads_does_not_duplicate_the_edge() {
    let mut fixture = Fixture::new();
    commit_fixture(&fixture);
    fixture.sync_and_build();
    fixture.sync_and_build();
    assert_eq!(correlated(&fixture).len(), 1);
    assert_eq!(
        fixture.count(
            "MATCH (:Episode {repository_id: $repo})-[r:AT_COMMIT]->(:Commit {repository_id: $repo}) RETURN count(r) AS n"
        ),
        1
    );
}
