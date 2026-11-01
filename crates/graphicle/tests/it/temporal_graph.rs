//! The bitemporal timeline reads of `graph.as_of` (`valid_at`, `known_at`,
//! `branch`), historical aliases and hyperedges. Assertions live in the
//! graph store (`Assertion`/`Alias`/`Hyperedge` nodes); the projection rows
//! and episode registry the views resolve against are still written by
//! `graph_projection::persist` (episodes are graph-native
//! the projection rewrite lands).
use graphicle::graph::Graph;
use graphicle_runtime::{
    graph_projection,
    store::{Store, s},
    temporal_graph,
};
use serde_json::{Value, json};

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Temporal proof").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = store
            .register(
                &workspace,
                &key,
                &json!({"project_key":"temporal","name":"Temporal"}),
            )
            .unwrap();
        Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo: s(&repo, "id").into(),
        }
    }
    fn episode(&self, content: &str) -> String {
        self.store
            .transaction(|db| {
                let sha = graphicle_memory::text::sha256(content.as_bytes());
                let project = graphicle_runtime::store::rows(db, "SELECT * FROM repo_projects WHERE id=?", rusqlite::params![self.repo])?.pop().unwrap();
                graphicle_runtime::store_content::put_objects(&self.store.graph(), &project, &json!([{"sha256":sha,"content":content,"bytes":content.len()}]))?;
                graphicle_runtime::store_content::put_head(&self.store.graph(), &project, "a.py", &sha)?;
                let episode = graph_projection::current_episode(db, &self.repo)?;
                let graph = Graph::from_node_link(&json!({"directed":true,"multigraph":true,"nodes":[{"id":"a","label":content,"stable_id":"a"},{"id":"b","label":"b","stable_id":"b"}],"links":[{"source":"a","target":"b","key":"calls","relation":"calls"}]}), "links")?;
                graph_projection::persist(db, &self.store.graph(), &self.repo, &episode, &graph)?;
                Ok(episode)
            })
            .unwrap()
    }
    fn view(&self, args: Value) -> Value {
        self.store
            .graph_as_of(&self.workspace, &self.key, &self.repo, &args)
            .unwrap()
    }
}

#[test]
fn retroactive_correction_preserves_what_was_known_and_branch_scope() {
    // A later assertion corrects the same valid time; what was known before
    // it was recorded stays readable.
    let f = Fixture::new();
    let a = f.episode("def a(): return 1\n");
    let b = f.episode("def a(): return 2\n");
    let (first, second) = ("2021-01-01T00:00:00Z", "2021-01-02T00:00:00Z");
    let at = |s| temporal_graph::instant(s).unwrap();
    f.store
        .graph()
        .transaction(|db| {
            for (episode, recorded) in [(&a, first), (&b, second)] {
                temporal_graph::observe(
                    db,
                    &f.repo,
                    episode,
                    "release",
                    at("2020-01-01T00:00:00Z"),
                    at(recorded),
                    "explicit_assertion",
                )?;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(
        f.view(json!({"branch":"release","valid_at":"2020-01-02T00:00:00Z","known_at":first}))["episode"],
        a
    );
    assert_eq!(
        f.view(json!({"branch":"release","valid_at":"2020-01-02T00:00:00Z","known_at":second}))["episode"],
        b
    );
    assert!(
        f.store
            .graph_as_of(
                &f.workspace,
                &f.key,
                &f.repo,
                &json!({"branch":"other","valid_at":"2020-01-02T00:00:00Z"})
            )
            .is_err()
    );
    assert!(
        f.store
            .graph_as_of(
                &f.workspace,
                &f.key,
                &f.repo,
                &json!({"as_of":a,"known_at":first})
            )
            .is_err()
    );
}

#[test]
fn recurring_graphs_preserve_the_gap_and_exact_time_zone_boundaries() {
    // Graphiti #1865/#1864: recurrence must not reuse an ended occurrence,
    // and an equivalent UTC offset must select the same boundary.
    let f = Fixture::new();
    let a = f.episode("def a(): return 1\n");
    let b = f.episode("def a(): return 2\n");
    let at = |s| temporal_graph::instant(s).unwrap();
    f.store
        .graph()
        .transaction(|db| {
            for (episode, valid, known) in [
                (&a, "2020-01-01T00:00:00Z", "2020-01-01T00:00:00Z"),
                (&b, "2020-01-10T00:00:00Z", "2020-01-12T00:00:00Z"),
                (&a, "2020-01-20T00:00:00Z", "2020-01-22T00:00:00Z"),
            ] {
                temporal_graph::observe(
                    db,
                    &f.repo,
                    episode,
                    "recurrence",
                    at(valid),
                    at(known),
                    "fixture",
                )?;
            }
            Ok(())
        })
        .unwrap();
    for (valid, expected) in [
        ("2020-01-05T00:00:00Z", &a),
        ("2020-01-19T23:59:59.999999Z", &b),
        ("2020-01-20T00:00:00Z", &a),
        ("2020-01-20T05:30:00+05:30", &a),
    ] {
        assert_eq!(
            f.view(json!({"branch":"recurrence", "valid_at":valid,
            "known_at":"2020-02-01T00:00:00Z"}))["episode"],
            *expected
        );
    }
    assert_eq!(
        f.view(
            json!({"branch":"recurrence", "valid_at":"2020-01-25T00:00:00Z",
        "known_at":"2020-01-21T23:59:59.999999Z"})
        )["episode"],
        b
    );
}

#[test]
fn historical_aliases_and_hyperedges_survive_retraction_without_future_leakage() {
    let f = Fixture::new();
    let episode = f.episode("def a(): return 1\n");
    let at = temporal_graph::instant("2020-01-01T00:00:00Z").unwrap();
    f.store
        .graph()
        .transaction(|db| {
            let proposal = json!({"id":"p","from_node":"a","to_node":"b"});
            temporal_graph::alias_event(db, &f.repo, &proposal, true, at, at + 10)?;
            temporal_graph::alias_event(db, &f.repo, &proposal, false, at, at + 20)?;
            assert!(temporal_graph::aliases_at(db, &f.repo, at, at + 9)?.is_empty());
            assert!(!temporal_graph::aliases_at(db, &f.repo, at, at + 10)?.is_empty());
            assert!(temporal_graph::aliases_at(db, &f.repo, at, at + 20)?.is_empty());
            Ok(())
        })
        .unwrap();
    // A hyperedge created, then retracted, at the same valid time.
    let (created, retracted) = (at + 10, at + 20);
    f.store
        .graph()
        .transaction(|db| {
            let edge = json!({"hyperedge_id":"contract","episode":episode,"relation":"requires",
                "members":["a","b"],"attributes":{}});
            for (active, recorded) in [(true, created), (false, retracted)] {
                temporal_graph::hyperedge_event(db, &f.repo, &edge, active, at, recorded)?;
            }
            Ok(())
        })
        .unwrap();
    let graph = f
        .store
        .transaction(|db| {
            let episode = graph_projection::current_episode(db, &f.repo)?;
            graph_projection::load(&crate::gstore(db), &f.repo, &episode)
        })
        .unwrap();
    let known = |recorded| {
        f.store
            .graph()
            .read(|db| temporal_graph::hyperedges_at(db, &f.repo, at, recorded, &graph))
    };
    assert!(known(created - 1).unwrap().0.is_empty());
    assert_eq!(known(created).unwrap().0.len(), 1);
    assert!(known(retracted).unwrap().0.is_empty());
}

#[test]
fn malformed_temporal_selectors_are_rejected_and_hyperedges_report_truncation() {
    let f = Fixture::new();
    let episode = f.episode("def a(): return 1\n");
    for args in [
        json!({"valid_at":1}),
        json!({"known_at":null}),
        json!({"branch":1}),
        json!({"branch":""}),
        json!({"canonical":"yes"}),
        json!({"as_of":false}),
        json!({"valid_at":"2020-01-01"}),
        json!({"branch":"bad\nbranch"}),
    ] {
        assert!(
            f.store
                .graph_as_of(&f.workspace, &f.key, &f.repo, &args)
                .is_err(),
            "{args}"
        );
    }
    f.store
        .graph()
        .transaction(|db| {
            for i in 0..1001 {
                let edge = json!({"hyperedge_id":format!("h{i:04}"),"episode":episode,
                    "relation":"requires","members":["a","b"],"attributes":{}});
                temporal_graph::hyperedge_event(db, &f.repo, &edge, true, 0, 0)?;
            }
            Ok(())
        })
        .unwrap();
    let view = f.view(json!({}));
    assert_eq!(view["hyperedges"].as_array().unwrap().len(), 1000);
    assert_eq!(view["hyperedges_truncated"], true);
    assert_eq!(view["hyperedges"][0]["unresolved_members"], json!([]));
    assert_eq!(view["hyperedges"][0]["hyperedge_id"], "h0000");
}
