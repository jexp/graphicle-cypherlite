//! The enrich job (architecture-concepts.md §1.5 and §5): a projection that
//! records an episode for a project with rules queues the rule set's
//! application as `(:Job {kind: 'enrich'})`, keyed by (rule set, episode);
//! the drain runs it after the projection; a project without rules queues
//! nothing; a new episode re-runs the rule set with its own application
//! records.
use graphicle_runtime::{
    arch_rules,
    jobs,
    local,
    store::{Store, s},
    store_graph::{self, GraphStore, rows},
};
use serde_json::{Map, Value, json};
use tempfile::TempDir;

const RULES: &str = "# Architecture rules\n\n\
Every symbol of the project belongs to the test concept.\n\n\
````concept\nid: arch:Test/Label\nseverity: MINOR\ndescription: Every symbol is test material\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) SET s:ArchTest\n```\n````\n\
````constraint\nid: arch:Test/Ungrouped\nseverity: MAJOR\nrequires: [arch:Test/Label]\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) WHERE NOT s:ArchTest RETURN s\n```\n````\n";

struct Fixture {
    _dir: TempDir,
    root: std::path::PathBuf,
    store: Store,
    session: local::Session,
    repo: String,
}

impl Fixture {
    fn new() -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn run() {}\n").unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        let mut session = local::session(&store, true).unwrap();
        let synced = local::sync(&store, &mut session, &root, None).unwrap();
        let repo = s(&synced, "repository_id").to_string();
        Self {
            _dir: dir,
            root,
            store,
            session,
            repo,
        }
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn sync(&mut self) -> Value {
        local::sync(&self.store, &mut self.session, &self.root, Some(&self.repo)).unwrap()
    }

    /// Sync and build the graph, as `graphicle sync` does: the drain runs
    /// the projection job and the enrich job it queued.
    fn sync_and_build(&mut self) -> Value {
        self.sync();
        local::build_graph(&self.store, &self.session, &self.repo, false).unwrap()
    }

    fn graph(&self) -> GraphStore {
        (*self.store.graph()).clone()
    }

    /// Rows of a Cypher probe over the `$repo`-scoped graph store, with
    /// extra parameters merged in.
    fn query(&self, cypher: &str, extra: Value) -> Vec<Value> {
        let mut params = Map::new();
        params.insert("repo".into(), json!(self.repo));
        for (key, value) in extra.as_object().into_iter().flatten() {
            params.insert(key.clone(), value.clone());
        }
        self.graph()
            .read(|db| rows(db, cypher, store_graph::params(&Value::Object(params))))
            .unwrap()
    }

    fn count(&self, cypher: &str) -> i64 {
        self.query(cypher, json!({}))[0]["n"].as_i64().unwrap()
    }

    /// The enrich jobs of the project, oldest first: `{id, state,
    /// input_hash, episode, result}` with `result` decoded.
    fn enrich_jobs(&self) -> Vec<Value> {
        self.query(
            "MATCH (j:Job {repository_id: $repo, kind: 'enrich'}) \
             RETURN j.id AS id, j.state AS state, j.input_hash AS input_hash, \
                    j.episode AS episode, j.result AS result, j.attempt AS attempt, \
                    j.max_attempts AS max_attempts \
             ORDER BY j.created_at",
            json!({}),
        )
        .into_iter()
        .map(|mut job| {
            job["result"] =
                serde_json::from_str::<Value>(s(&job, "result")).unwrap_or(Value::Null);
            job
        })
        .collect()
    }

    /// The recorded episodes, oldest first.
    fn episodes(&self) -> Vec<String> {
        self.query(
            "MATCH (e:Episode {repository_id: $repo}) RETURN e.episode AS episode \
             ORDER BY e.sequence",
            json!({}),
        )
        .into_iter()
        .map(|row| s(&row, "episode").to_string())
        .collect()
    }

    /// How many symbols the label rule's application of `episode` recorded.
    fn labeled_by(&self, rule: &str, episode: &str) -> i64 {
        self.query(
            "MATCH (:RuleApplication {repository_id: $repo, rule: $rule, episode: $ep})-[:LABELED]->(s:Symbol) \
             RETURN count(s) AS n",
            json!({"rule": rule, "ep": episode}),
        )[0]["n"]
            .as_i64()
            .unwrap()
    }
}

#[test]
fn a_projection_of_a_project_with_rules_queues_and_runs_the_enrich_job() {
    let mut f = Fixture::new();
    f.write("docs/architecture/rules.md", RULES);
    f.sync_and_build();

    // The projection job's result receipts the enrich job it queued.
    let projected = f
        .query(
            "MATCH (j:Job {repository_id: $repo, kind: 'project'}) RETURN j.result AS result \
             ORDER BY j.created_at DESC",
            json!({}),
        )
        .into_iter()
        .next()
        .unwrap();
    let result: Value = serde_json::from_str(s(&projected, "result")).unwrap();
    assert_eq!(result["enrich"]["queued"], json!(true), "{result:#}");

    // One enrich job, keyed by the rule set and the episode, run to
    // completion through the queue.
    let jobs = f.enrich_jobs();
    assert_eq!(jobs.len(), 1, "{jobs:#?}");
    let job = &jobs[0];
    assert_eq!(job["state"], json!("complete"), "{job:#}");
    assert_eq!(job["attempt"], json!(1), "{job:#}");
    let episode = f.episodes().pop().unwrap();
    assert_eq!(job["episode"], json!(episode), "{job:#}");
    let rule_set = arch_rules::rule_set_hash(&f.graph(), &f.repo).unwrap();
    assert_eq!(
        job["input_hash"],
        json!(jobs::enrich_input_hash(&rule_set, &episode)),
        "keyed by the rule set hash and the episode"
    );

    // The concept ran: the symbols carry its label, and the constraint's
    // effective severity is carried through for the later evaluator.
    let result = &job["result"];
    assert_eq!(result["applied"], json!(1), "{result:#}");
    assert_eq!(result["failed"], json!(0), "{result:#}");
    assert_eq!(
        result["plans"][0]["severities"]["arch:Test/Ungrouped"],
        json!("MAJOR"),
        "{result:#}"
    );
    assert!(f.count("MATCH (s:Symbol:ArchTest) RETURN count(s) AS n") > 0);

    // The constraint runs after the concept over the enriched graph: every
    // symbol carries the label, so it returns no rows and is passing.
    assert_eq!(result["constraints"]["passing"], json!(1), "{result:#}");
    assert_eq!(result["constraints"]["violations"], json!(0), "{result:#}");
    assert_eq!(
        f.count("MATCH (v:Violation {repository_id: $repo}) RETURN count(v) AS n"),
        0
    );
    assert_eq!(
        f.count("MATCH (a:RuleApplication {rule: 'arch:Test/Label'}) RETURN count(a) AS n"),
        1
    );

    // A repeated completion of the same (rule set, episode) replays the job
    // rather than queueing another.
    f.sync_and_build();
    assert_eq!(f.enrich_jobs().len(), 1, "the same job, replayed");
}

#[test]
fn a_projection_of_a_project_without_rules_queues_no_enrich_job() {
    let mut f = Fixture::new();
    f.sync_and_build();
    assert_eq!(
        f.count("MATCH (j:Job {repository_id: $repo, kind: 'enrich'}) RETURN count(j) AS n"),
        0
    );
    assert_eq!(f.count("MATCH (a:RuleApplication) RETURN count(a) AS n"), 0);
}

#[test]
fn a_new_episode_re_runs_the_rule_set_with_its_own_applications() {
    let mut f = Fixture::new();
    f.write("docs/architecture/rules.md", RULES);
    f.sync_and_build();
    let first_symbols = f.labeled_by("arch:Test/Label", &f.episodes()[0]);

    // A changed tree is a new head set: the next sync projects it as a new
    // episode and the rule set runs again, against the new episode's graph.
    f.write("src/more.rs", "pub fn later() {}\n");
    f.sync_and_build();

    let episodes = f.episodes();
    assert_eq!(episodes.len(), 2, "{episodes:?}");
    let (first, second) = (episodes[0].clone(), episodes[1].clone());

    // Two enrich jobs, one per (rule set, episode).
    let jobs = f.enrich_jobs();
    assert_eq!(jobs.len(), 2, "{jobs:#?}");
    assert_eq!(jobs[0]["episode"], json!(first));
    assert_eq!(jobs[1]["episode"], json!(second));
    assert_ne!(jobs[0]["input_hash"], jobs[1]["input_hash"]);

    // The second episode's enrichment was recomputed, not copied: its
    // application of the label rule recorded the symbols the new episode
    // added — the new file's — which the first episode's records cannot
    // know, and the first episode's records still stand on their own.
    assert!(
        f.labeled_by("arch:Test/Label", &second) > 0,
        "the new episode's symbols are the second run's writes"
    );
    assert_eq!(
        f.labeled_by("arch:Test/Label", &first),
        first_symbols,
        "the first episode's application records are unchanged"
    );
    assert!(
        f.query(
            "MATCH (:RuleApplication {repository_id: $repo, rule: 'arch:Test/Label', episode: $ep})-[:LABELED]->(:Symbol {relpath: 'src/more.rs'}) \
             RETURN count(*) AS n",
            json!({"ep": second}),
        )[0]["n"]
            .as_i64()
            .unwrap()
            > 0,
        "the new file's symbol was labelled by the second run"
    );
    assert!(
        f.query(
            "MATCH (:RuleApplication {repository_id: $repo, rule: 'arch:Test/Label', episode: $ep})-[:LABELED]->(:Symbol {relpath: 'src/more.rs'}) \
             RETURN count(*) AS n",
            json!({"ep": first}),
        )[0]["n"]
            .as_i64()
            .unwrap()
            == 0,
        "the first episode's application cannot name a symbol it never saw"
    );

    // The episodes' records are distinguishable: one application of the
    // rule per episode.
    for episode in [&first, &second] {
        assert_eq!(
            f.query(
                "MATCH (a:RuleApplication {repository_id: $repo, rule: 'arch:Test/Label', episode: $ep}) \
                 RETURN count(a) AS n",
                json!({"ep": episode}),
            )[0]["n"],
            json!(1)
        );
    }

    // Both episodes' symbols carry the concept's label now.
    assert!(
        f.count(
            "MATCH (s:Symbol {relpath: 'src/more.rs'}) WHERE s:ArchTest RETURN count(s) AS n"
        ) > 0
    );
}
