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
        latest["state"], json!("complete"),
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
