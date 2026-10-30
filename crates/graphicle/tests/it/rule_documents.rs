//! Rule documents at sync (architecture-concepts.md §1.5): a Markdown rule
//! document under `docs/architecture/` or `.graphicle/` gives the project
//! its rule nodes after one sync, a project without one writes none, and a
//! changed document is parsed again at the next sync.
use graphicle_runtime::{
    arch_rules, local,
    store::{Store, s},
    store_graph::{self, GraphStore, Params, rows},
};
use serde_json::{Value, json};
use tempfile::TempDir;

const LAYERS: &str = "# Layers\n\n````concept\nid: arch:Layer/Persistence\nseverity: MINOR\n```cypher\nMATCH (m:Module) RETURN m\n```\n````\n\n````constraint\nid: arch:NoCycles\nseverity: BLOCKER\n```cypher\nMATCH p=(m:Module)-[:DEPENDS_ON*2..6]->(m) RETURN p\n```\n````\n";

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

    /// Copy the jQA AsciiDoc fixture (`git.adoc` and its `queries/`) into the
    /// project's `docs/architecture/`, so its includes resolve beside it.
    fn write_adoc_fixture(&self) {
        let from = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/arch_rules/jqa-git");
        let to = self.root.join("docs/architecture");
        std::fs::create_dir_all(to.join("queries")).unwrap();
        std::fs::copy(from.join("git.adoc"), to.join("git.adoc")).unwrap();
        for query in ["authors.adoc", "churn.adoc", "coupling.adoc"] {
            std::fs::copy(
                from.join("queries").join(query),
                to.join("queries").join(query),
            )
            .unwrap();
        }
    }

    fn sync(&mut self) -> Value {
        local::sync(&self.store, &mut self.session, &self.root, Some(&self.repo)).unwrap()
    }

    fn graph(&self) -> GraphStore {
        (*self.store.graph()).clone()
    }

    fn count(&self, label: &str) -> i64 {
        let cypher = format!("MATCH (r:{label} {{repository_id: $repo}}) RETURN count(r) AS n");
        let params = store_graph::params(&json!({"repo": self.repo}));
        self.graph()
            .read(|db| rows(db, &cypher, params.clone()))
            .unwrap()[0]["n"]
            .as_i64()
            .unwrap()
    }

    fn rule_shas(&self) -> Vec<String> {
        let mut shas: Vec<String> = self
            .graph()
            .read(|db| {
                rows(
                    db,
                    "MATCH (r:ConceptRule) RETURN r.rule_sha AS sha UNION ALL MATCH (r:ConstraintRule) RETURN r.rule_sha AS sha",
                    Params::new(),
                )
            })
            .unwrap()
            .iter()
            .map(|row| s(row, "sha").to_string())
            .collect();
        shas.sort();
        shas
    }
}

#[test]
fn a_rule_document_gives_the_project_its_rule_nodes_after_one_sync() {
    let mut f = Fixture::new();
    f.write("docs/architecture/layers.md", LAYERS);
    let synced = f.sync();
    assert_eq!(f.count("ConceptRule"), 1, "{synced:#}");
    assert_eq!(f.count("ConstraintRule"), 1, "{synced:#}");
    assert_eq!(synced["rules"]["parsed"], json!(1), "{synced:#}");
}

#[test]
fn an_asciidoc_rule_document_gives_the_project_its_rule_nodes_after_one_sync() {
    let mut f = Fixture::new();
    f.write_adoc_fixture();
    let synced = f.sync();
    assert!(f.count("ConceptRule") > 0, "{synced:#}");
    assert_eq!(f.count("RuleGroup"), 1, "{synced:#}");
    assert_eq!(synced["rules"]["parsed"], json!(1), "{synced:#}");
    assert_eq!(synced["rules"]["skipped"], json!([]), "{synced:#}");
}

#[test]
fn a_markdown_and_an_asciidoc_document_both_ingest() {
    let mut f = Fixture::new();
    f.write("docs/architecture/layers.md", LAYERS);
    f.write_adoc_fixture();
    let synced = f.sync();
    // Two documents at the top of docs/architecture/: layers.md and git.adoc.
    // The queries/ files are included by git.adoc, not ingested themselves.
    assert_eq!(synced["rules"]["parsed"], json!(2), "{synced:#}");
    // layers.md holds one constraint; git.adoc's includes hold two more
    // (churn.adoc, coupling.adoc). LAYERS has no group; the git group is the
    // only one, from git.adoc.
    assert_eq!(f.count("ConstraintRule"), 3, "{synced:#}");
    assert_eq!(f.count("RuleGroup"), 1, "{synced:#}");
}

#[test]
fn a_rule_document_under_graphicle_folder_is_ingested() {
    let mut f = Fixture::new();
    f.write(".graphicle/rules.md", LAYERS);
    f.sync();
    assert_eq!(f.count("ConceptRule"), 1);
}

#[test]
fn a_project_without_a_rule_document_writes_nothing_rule_related() {
    let mut f = Fixture::new();
    let synced = f.sync();
    assert!(synced.get("rules").is_none(), "{synced:#}");
    assert_eq!(f.count("ConceptRule"), 0);
    assert_eq!(f.count("ConstraintRule"), 0);
    assert_eq!(f.count("RuleGroup"), 0);
}

#[test]
fn an_edited_rule_document_is_parsed_again_at_the_next_sync() {
    let mut f = Fixture::new();
    f.write("docs/architecture/layers.md", LAYERS);
    f.sync();
    let before = f.rule_shas();

    f.write(
        "docs/architecture/layers.md",
        &LAYERS.replace("severity: BLOCKER", "severity: CRITICAL"),
    );
    let synced = f.sync();
    assert_eq!(synced["rules"]["parsed"], json!(1), "{synced:#}");
    assert_ne!(f.rule_shas(), before, "rule_sha must move with the text");
    assert_eq!(f.count("ConstraintRule"), 1);
}

#[test]
fn an_unchanged_rule_document_is_not_parsed_again() {
    let mut f = Fixture::new();
    f.write("docs/architecture/layers.md", LAYERS);
    f.sync();
    let synced = f.sync();
    assert_eq!(synced["rules"]["parsed"], json!(0), "{synced:#}");
    assert_eq!(f.count("ConceptRule"), 1);
}

#[test]
fn a_removed_rule_document_loses_its_rules() {
    let mut f = Fixture::new();
    f.write("docs/architecture/layers.md", LAYERS);
    f.sync();
    std::fs::remove_file(f.root.join("docs/architecture/layers.md")).unwrap();
    let synced = f.sync();
    assert_eq!(synced["rules"]["removed"], json!(1), "{synced:#}");
    assert_eq!(f.count("ConceptRule"), 0);
    assert_eq!(f.count("ConstraintRule"), 0);
}

#[test]
fn a_document_with_no_rules_records_its_digest_and_is_not_parsed_again() {
    let mut f = Fixture::new();
    f.write("docs/architecture/notes.md", "# Notes\n\nNo rules here.\n");
    let first = f.sync();
    assert_eq!(first["rules"]["parsed"], json!(1), "{first:#}");
    assert_eq!(f.count("ConceptRule"), 0);
    let again = f.sync();
    assert_eq!(again["rules"]["parsed"], json!(0), "{again:#}");
}

#[test]
fn an_unparseable_document_is_listed_as_skipped_and_writes_no_rules() {
    let mut f = Fixture::new();
    f.write(
        "docs/architecture/broken.md",
        "````concept\nid: arch:Open\n",
    );
    let synced = f.sync();
    assert_eq!(
        synced["rules"]["skipped"][0]["path"],
        json!("docs/architecture/broken.md"),
        "{synced:#}"
    );
    assert_eq!(f.count("ConceptRule"), 0);
}

#[test]
fn the_rule_set_hash_is_the_digest_of_the_sorted_rule_shas() {
    let mut f = Fixture::new();
    f.write("docs/architecture/layers.md", LAYERS);
    f.sync();
    let shas = f.rule_shas();
    let expected = graphicle_memory::text::sha256(shas.join("\n").as_bytes());
    let hash = graphicle_runtime::arch_rules::rule_set_hash(&f.graph(), &f.repo).unwrap();
    assert_eq!(hash, expected);
}
