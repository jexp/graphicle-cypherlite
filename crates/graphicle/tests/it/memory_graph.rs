//! The memory pipeline on the graph store (`cypherlite-backend`,
//! memory-pipeline-cypher): memories, revisions, claims with their entity
//! links, passage embeddings on `Vector`-typed node properties, and the
//! cascade deletion of all of them.
use graphicle_runtime::{
    claims, deletion, memory,
    store::{Store, repo, s},
    store_graph, vector,
};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let w = store.create_workspace("Memory graph").unwrap();
        let r = store
            .register(
                s(&w, "id"),
                s(&w, "capability"),
                &json!({"project_key":"memory","name":"Memory"}),
            )
            .unwrap();
        Self {
            _dir: dir,
            workspace: s(&w, "id").into(),
            key: s(&w, "capability").into(),
            store,
            repo: s(&r, "id").into(),
        }
    }

    fn args(&self, mut value: Value) -> Value {
        value["repository_id"] = json!(self.repo);
        value
    }

    fn save(&self, id: Option<&str>, expected: Option<i64>, title: &str, body: &str) -> Value {
        self.store
            .authorized(&self.workspace, &self.key, "approve", |db, _| {
                memory::save(
                    db,
                    &self.store.graph(),
                    &self.repo,
                    id,
                    expected,
                    "note",
                    title,
                    body,
                    "reviewed",
                    &json!({"dependencies":{},"evidence_links":{},"native_sources":{},"notes":{}}),
                )
            })
            .unwrap()
    }

    fn sync(&self, files: &[(&str, &str)]) {
        let status = self
            .store
            .project_status(&self.workspace, &self.key, &self.args(json!({})))
            .unwrap();
        let manifest: Vec<_> = files
            .iter()
            .map(|(name, text)| {
                json!({"path":name,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()})
            })
            .collect();
        let prepare = self
            .store
            .sync_prepare(
                &self.workspace,
                &self.key,
                &self.args(json!({"manifest":manifest,"expected_snapshot":status["repository"]["snapshot"]})),
            )
            .unwrap();
        if prepare["replayed"] == true {
            return;
        }
        if !files.is_empty() {
            self.store
                .sync_upload(
                    &self.workspace,
                    &self.key,
                    &self.args(json!({"sync_id":prepare["sync_id"],"files":files.iter().map(|(path,content)|json!({"path":path,"content":content})).collect::<Vec<_>>()})),
                )
                .unwrap();
        }
        self.store
            .sync_commit(
                &self.workspace,
                &self.key,
                &self.args(json!({"sync_id":prepare["sync_id"]})),
            )
            .unwrap();
    }

    fn memories(&self) -> Vec<Value> {
        self.store
            .authorized(&self.workspace, &self.key, "read", |db, p| {
                let repository = repo(db, p, &self.repo)?;
                memory::list(db, &self.store.graph(), &repository, p, true)
            })
            .unwrap()
    }
}

#[test]
fn memories_and_revisions_live_in_the_graph() {
    let f = Fixture::new();
    let saved = f.save(None, None, "Aquarium", "The tank holds six neon tetras.");
    let id = s(&saved, "id").to_string();
    assert_eq!(saved["revision"], 1);

    // A stale expected revision conflicts; the current one bumps.
    let stale = f
        .store
        .authorized(&f.workspace, &f.key, "approve", |db, _| {
            memory::save(
                db,
                &f.store.graph(),
                &f.repo,
                Some(&id),
                Some(9),
                "note",
                "Aquarium",
                "x",
                "reviewed",
                &json!({"dependencies":{},"evidence_links":{},"native_sources":{},"notes":{}}),
            )
        });
    assert!(stale.is_err());
    let bumped = f.save(
        Some(&id),
        Some(1),
        "Aquarium",
        "The tank holds seven tetras.",
    );
    assert_eq!(bumped["revision"], 2);

    let memories = f.memories();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0]["body"], "The tank holds seven tetras.");
    // The SQL table shape: decoded data, no engine bookkeeping or embeddings.
    assert!(memories[0]["data"].is_object());
    assert!(memories[0].get("_id").is_none());
    assert!(memories[0].get("embedding").is_none());

    // Both revisions are journaled, in order, as stringified rows.
    let revisions = f
        .store
        .graph()
        .read(|g| {
            store_graph::rows(
                g,
                "MATCH (r:MemoryRevision {memory_id: $id})-[:REVISION_OF]->(m:Memory) \
                 RETURN r.revision AS revision, r.data AS data, m.id AS memory \
                 ORDER BY r.revision",
                store_graph::params(&json!({"id": id})),
            )
        })
        .unwrap();
    assert_eq!(revisions.len(), 2);
    let journaled: Value = serde_json::from_str(s(&revisions[1], "data")).unwrap();
    assert_eq!(journaled["body"], "The tank holds seven tetras.");

    // The fulltext index over title/body maintains itself.
    let hits = f
        .store
        .graph()
        .read(|g| {
            store_graph::rows(
                g,
                "MATCH (m:Memory) SEARCH m IN (FULLTEXT INDEX mem_text FOR 'tetras' LIMIT 5) \
                 RETURN m.id AS id",
                store_graph::params(&json!({})),
            )
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(s(&hits[0], "id"), id);
}

#[test]
fn claims_entities_and_links_live_in_the_graph() {
    let f = Fixture::new();
    f.sync(&[("notes.txt", "Mira prefers quiet rooms.")]);
    let draft: String = f
        .store
        .authorized(&f.workspace, &f.key, "approve", |db, p| {
            let repository = repo(db, p, &f.repo)?;
            let docs = memory::documents(db, &repository, p)?;
            let sources: Vec<Value> = docs
                .iter()
                .map(|d| json!({"id":d["id"],"text":d["content"],"sha256":d["sha256"]}))
                .collect();
            let claim = json!({"statement":"Mira prefers quiet rooms.","subject":"Mira",
                "relation":"prefers","object":"quiet rooms","speaker":"user","modality":"preference",
                "conditions":[],"action":"ADD","target_id":null,"valid_at":null,"invalid_at":null,
                "time_basis":"unknown","anchor_id":null,
                "evidence":[{"id":"file:notes.txt","quote":"Mira prefers quiet rooms."}]});
            let result = claims::propose(
                db,
                &f.store.graph(),
                &repository,
                p,
                &json!([claim]),
                &sources,
                &[],
                &json!({"model":"probe"}),
            )?;
            Ok(s(&result["drafts"][0], "memory_id").to_string())
        })
        .unwrap();
    f.store
        .review_memory(
            &f.workspace,
            &f.key,
            &f.args(
                json!({"memory_id":draft,"expected_revision":1,"decision":"approve",
                "maintainer":"qa","reason":"ok"}),
            ),
        )
        .unwrap();
    let claims = f
        .store
        .authorized(&f.workspace, &f.key, "read", |db, p| {
            let repository = repo(db, p, &f.repo)?;
            claims::current(db, &f.store.graph(), &repository, p)
        })
        .unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0]["state"], "active");
    let links = f
        .store
        .graph()
        .read(|g| {
            store_graph::rows(
                g,
                "MATCH (c:Claim)-[:HAS_SUBJECT]->(s:Entity) \
                 MATCH (c)-[:HAS_OBJECT]->(o:Entity) \
                 RETURN s.name AS subject, o.name AS object",
                store_graph::params(&json!({})),
            )
        })
        .unwrap();
    assert_eq!(
        links,
        vec![json!({"subject":"Mira","object":"quiet rooms"})]
    );
    // The approval event is journaled on the claim.
    let events = f
        .store
        .graph()
        .read(|g| {
            store_graph::rows(
                g,
                "MATCH (e:ClaimEvent)-[:FOR]->(c:Claim) RETURN e.action AS action",
                store_graph::params(&json!({})),
            )
        })
        .unwrap();
    assert_eq!(events, vec![json!({"action":"ADD"})]);
}

#[test]
fn embeddings_live_on_nodes_and_rank_through_the_vector_index() {
    let f = Fixture::new();
    let saved = f.save(None, None, "Aquarium", "The tank holds six neon tetras.");
    let memory_identity = format!("memory:{}", s(&saved, "id"));
    let rows = vec![
        (
            memory_identity.clone(),
            "sha-m".to_string(),
            vec![1.0, 0.0, 0.0],
        ),
        (
            "file:notes.txt:0".to_string(),
            "sha-c".to_string(),
            vec![0.0, 1.0, 0.0],
        ),
    ];
    vector::store_embeddings(&f.store.graph(), &f.repo, "test-model", &rows).unwrap();
    assert_eq!(
        vector::graph_vector_count(&f.store.graph(), &f.repo).unwrap(),
        2
    );
    let index = vector::graph_vector_index(&f.store.graph(), &f.repo, "test-model").unwrap();
    assert_eq!(index.len(), 2);

    // The nearest embedding to a query vector ranks first, through SEARCH.
    let hits = vector::semantic_search(
        &f.store.graph(),
        &f.repo,
        "test-model",
        &[0.9, 0.1, 0.0],
        10,
    )
    .unwrap();
    assert_eq!(hits[0].0, memory_identity);
    assert!(hits[0].2 > hits[1].2, "{hits:?}");
    // The memory listing does not carry the vector payload.
    assert!(f.memories()[0].get("embedding").is_none());
    // A wrong model key sees nothing.
    assert!(
        vector::graph_vector_index(&f.store.graph(), &f.repo, "other-model")
            .unwrap()
            .is_empty()
    );

    // Deleting the passage's embedding drops the stub chunk; deleting the
    // memory's removes the property.
    let removed = vector::delete_embeddings(
        &f.store.graph(),
        &f.repo,
        &["file:notes.txt:0".to_string(), memory_identity.clone()],
    )
    .unwrap();
    assert_eq!(removed, 2);
    assert_eq!(
        vector::graph_vector_count(&f.store.graph(), &f.repo).unwrap(),
        0
    );
}

#[test]
fn forgetting_a_connector_takes_its_memories_claims_and_embeddings() {
    let f = Fixture::new();
    let connector = f
        .store
        .connector_add(
            &f.workspace,
            &f.key,
            &f.args(json!({"kind":"jsonl","name":"Chat","config":{"acl":{"workspace_readers":true,"credentials":[]}}})),
        )
        .unwrap();
    let connector = s(&connector, "id").to_string();
    f.store
        .import_documents(
            &f.workspace,
            &f.key,
            &f.args(json!({"connector_id":connector,"documents":[
                {"id":"c1","title":"Session c1","content":"Mira prefers quiet rooms."}
            ]})),
        )
        .unwrap();
    let source = format!("connector:{connector}:c1");
    let sha = graphicle_memory::text::sha256(b"Mira prefers quiet rooms.");
    let saved = f
        .store
        .authorized(&f.workspace, &f.key, "approve", |db, _| {
            memory::save(
                db,
                &f.store.graph(),
                &f.repo,
                None,
                None,
                "note",
                "Preference",
                "Mira prefers quiet rooms.",
                "reviewed",
                &json!({"dependencies":{},"evidence_links":{},"native_sources":{source.clone():sha},"notes":{}}),
            )
        })
        .unwrap();
    vector::store_embeddings(
        &f.store.graph(),
        &f.repo,
        "test-model",
        &[(format!("{source}:0"), sha.clone(), vec![1.0, 0.0])],
    )
    .unwrap();
    assert_eq!(f.memories().len(), 1);
    assert_eq!(
        vector::graph_vector_count(&f.store.graph(), &f.repo).unwrap(),
        1
    );

    let result = f
        .store
        .maintain(
            &f.workspace,
            &f.key,
            "connector.delete",
            &f.args(json!({"connector_id": connector})),
        )
        .unwrap();
    assert_eq!(result["receipt"]["verified_local"], true, "{result:#}");
    assert!(f.memories().is_empty(), "{:?}", f.memories());
    assert_eq!(
        vector::graph_vector_count(&f.store.graph(), &f.repo).unwrap(),
        0
    );
    // The memory id and title stay out of the receipt.
    let body = result["receipt"].to_string();
    assert!(!body.contains(s(&saved, "id")));
    assert!(!body.contains("Preference"));
}

#[test]
fn memory_delete_cascades_through_the_graph() {
    let f = Fixture::new();
    let saved = f.save(None, None, "Aquarium", "The tank holds six neon tetras.");
    let id = s(&saved, "id").to_string();
    vector::store_embeddings(
        &f.store.graph(),
        &f.repo,
        "test-model",
        &[(format!("memory:{id}"), "sha-m".to_string(), vec![1.0, 0.0])],
    )
    .unwrap();
    let result = f
        .store
        .project_mutate(
            &f.workspace,
            &f.key,
            "memory.delete",
            &f.args(json!({"memory_id": id})),
        )
        .unwrap();
    assert_eq!(result["deleted"], 1, "{result}");
    assert!(f.memories().is_empty());
    let remaining = f
        .store
        .graph()
        .read(|g| {
            store_graph::rows(
                g,
                "MATCH (n) WHERE n:Memory OR n:MemoryRevision OR n:Claim OR n:ClaimEvent \
                 RETURN count(n) AS n",
                store_graph::params(&json!({})),
            )
        })
        .unwrap();
    assert_eq!(remaining[0]["n"], 0);
    // The verification agrees the rows are gone.
    let left = f
        .store
        .graph()
        .read(|g| deletion::memory_rows(g, &f.repo, std::slice::from_ref(&id)))
        .unwrap();
    assert_eq!(left["repo_memories"], 0);
    assert_eq!(left["am_vectors"], 0);
}
