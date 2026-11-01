//! The fixture the architecture suites share (`arch_rules`, `arch_enrich`,
//! `rule_documents`): a project folder with one source file, synced into
//! its own store, with the graph and the `$repo`-scoped Cypher probes its
//! rules are read and checked through. Each suite keeps its own
//! suite-specific helpers in an `impl Fixture` block beside its tests.
use graphicle_runtime::{
    local,
    store::{Store, s},
    store_graph::{self, GraphStore, rows},
};
use serde_json::{Map, Value, json};
use tempfile::TempDir;

/// A project folder (`root`) synced into its own store, as `graphicle sync`
/// finds it: one `src/lib.rs`, its session and its repository id.
pub struct Fixture {
    _dir: TempDir,
    pub root: std::path::PathBuf,
    pub store: Store,
    pub session: local::Session,
    pub repo: String,
}

impl Fixture {
    pub fn new() -> Self {
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

    /// Overwrite `path` (project-relative) with `text`, its folders made.
    pub fn write(&self, path: &str, text: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Sync the project again.
    pub fn sync(&mut self) -> Value {
        local::sync(&self.store, &mut self.session, &self.root, Some(&self.repo)).unwrap()
    }

    /// Sync and build the graph, as `graphicle sync` does: the drain runs
    /// the projection job and the enrich job it queued.
    pub fn sync_and_build(&mut self) -> Value {
        self.sync();
        local::build_graph(&self.store, &self.session, &self.repo, false).unwrap()
    }

    /// The project's graph store.
    pub fn graph(&self) -> GraphStore {
        (*self.store.graph()).clone()
    }

    /// Rows of a Cypher probe over the `$repo`-scoped graph store, with
    /// extra parameters merged in.
    pub fn query(&self, cypher: &str, extra: Value) -> Vec<Value> {
        let mut params = Map::new();
        params.insert("repo".into(), json!(self.repo));
        for (key, value) in extra.as_object().into_iter().flatten() {
            params.insert(key.clone(), value.clone());
        }
        self.graph()
            .read(|db| rows(db, cypher, store_graph::params(&Value::Object(params))))
            .unwrap()
    }

    /// The first row's `n` of a `$repo`-scoped count query.
    pub fn count(&self, cypher: &str) -> i64 {
        self.query(cypher, json!({}))[0]["n"].as_i64().unwrap()
    }
}
