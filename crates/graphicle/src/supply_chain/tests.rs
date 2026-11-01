//! The lockfile writer's and the import resolver's tests, on in-memory
//! stores. The real lockfile is this crate's own `Cargo.lock`, read through
//! `CARGO_MANIFEST_DIR`, and the real import facts are the engine's Rust
//! extractor run on this crate's own source.

use super::lock::{self, FILE, ingest, parse, purl_at_version};
use super::resolve;
use crate::store_graph::{self, GraphStore};
use serde_json::{Value, json};
use std::path::Path;

const REPO: &str = "repo-a";

/// Three packages, one of each source kind: a git dependency, a path
/// dependency (no source line) and a registry one.
const FIXTURE: &str = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "forked",
 "local",
 "serde",
]

[[package]]
name = "forked"
version = "0.3.0"
source = "git+https://github.com/example/forked?branch=fix#0123456789abcdef0123456789abcdef01234567"

[[package]]
name = "local"
version = "0.2.0"

[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"
"#;

/// The same project after a re-lock: `serde` moves up, `newdep` arrives,
/// `local` leaves the dependency list of `app`.
const RELOCKED: &str = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "forked",
 "newdep",
 "serde",
]

[[package]]
name = "forked"
version = "0.3.0"
source = "git+https://github.com/example/forked?branch=fix#0123456789abcdef0123456789abcdef01234567"

[[package]]
name = "newdep"
version = "0.1.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "1111111111111111111111111111111111111111111111111111111111111111"

[[package]]
name = "serde"
version = "1.0.201"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "2222222222222222222222222222222222222222222222222222222222222222"
"#;

/// A project folder holding `text` as its `Cargo.lock`.
fn folder(text: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(FILE), text).unwrap();
    dir
}

/// A fresh in-memory store with `repository_id`'s project node.
fn graph_for(repository_id: &str) -> GraphStore {
    let graph = GraphStore::open_in_memory().unwrap();
    project(&graph, repository_id);
    graph
}

fn project(graph: &GraphStore, repository_id: &str) {
    graph
        .transaction(|db| {
            store_graph::write(
                db,
                "CREATE (p:Project {id: $id, workspace_id: $ws, project_key: $key})",
                p(&json!({"id": repository_id, "ws": "ws", "key": repository_id})),
            )?;
            Ok(())
        })
        .unwrap();
}

fn p(value: &Value) -> store_graph::Params {
    store_graph::params(value)
}

/// The first row of a query, or `Null`.
fn row(graph: &GraphStore, cypher: &str, params: Value) -> Value {
    graph
        .read(|db| store_graph::one(db, cypher, p(&params)))
        .unwrap()
        .unwrap_or(Value::Null)
}

/// The first row's `n` of a count query.
fn count(graph: &GraphStore, cypher: &str, params: Value) -> i64 {
    row(graph, cypher, params)["n"].as_i64().unwrap_or_default()
}

/// The store as one comparable value: every node's labels and properties,
/// every relationship's type and properties.
fn state(graph: &GraphStore) -> Value {
    graph
        .read(|db| {
            let nodes = store_graph::rows(
                db,
                "MATCH (n) RETURN labels(n) AS labels, properties(n) AS props",
                p(&json!({})),
            )?;
            let rels = store_graph::rows(
                db,
                "MATCH ()-[r]->() RETURN type(r) AS type, properties(r) AS props",
                p(&json!({})),
            )?;
            Ok(json!({"nodes": nodes, "rels": rels}))
        })
        .unwrap()
}

/// The package nodes of `repository_id`, by purl.
fn packages(graph: &GraphStore, repository_id: &str) -> i64 {
    count(
        graph,
        "MATCH (pk:Package {repository_id: $repo}) RETURN count(*) AS n",
        json!({"repo": repository_id}),
    )
}

/// The `PackageVersion` with `key` in `repository_id`, or `Null`.
fn version(graph: &GraphStore, repository_id: &str, key: &str) -> Value {
    row(
        graph,
        "MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: $key}) \
         RETURN pv.version AS version, pv.source_kind AS source_kind, \
                pv.source_url AS source_url, pv.source_rev AS source_rev",
        json!({"repo": repository_id, "key": key}),
    )
}

/// The number of `REQUIRES` edges from `from` to `to`.
fn requires(graph: &GraphStore, repository_id: &str, from: &str, to: &str) -> i64 {
    count(
        graph,
        "MATCH (a:PackageVersion {repository_id: $repo, purl_at_version: $from})\
         -[r:REQUIRES]->(b:PackageVersion {repository_id: $repo, purl_at_version: $to}) \
         RETURN count(*) AS n",
        json!({"repo": repository_id, "from": from, "to": to}),
    )
}

/// The number of `DESCRIBES` edges from the lockfile to `key`.
fn describes(graph: &GraphStore, repository_id: &str, key: &str) -> i64 {
    count(
        graph,
        "MATCH (f:File {repository_id: $repo, path: 'Cargo.lock'})\
         -[:DESCRIBES]->(pv:PackageVersion {repository_id: $repo, purl_at_version: $key}) \
         RETURN count(*) AS n",
        json!({"repo": repository_id, "key": key}),
    )
}

#[test]
fn purls_are_minted_per_plan_section_two() {
    assert_eq!(lock::purl("serde"), "pkg:cargo/serde");
    assert_eq!(
        purl_at_version("serde", "1.0.229"),
        "pkg:cargo/serde@1.0.229"
    );
    // Build metadata's `+` is percent-encoded in a purl version.
    assert_eq!(
        purl_at_version("toml", "1.1.8+spec-1.1.0"),
        "pkg:cargo/toml@1.1.8%2Bspec-1.1.0"
    );
}

#[test]
fn a_malformed_lockfile_is_an_error_and_writes_nothing() {
    assert!(parse("this is = [not a lockfile").is_err());
    let dir = folder("this is = [not a lockfile");
    let graph = graph_for(REPO);
    assert!(ingest(&graph, REPO, dir.path()).is_err());
    assert_eq!(packages(&graph, REPO), 0);
}

#[test]
fn a_folder_without_a_lockfile_ingests_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let graph = graph_for(REPO);
    assert!(ingest(&graph, REPO, dir.path()).unwrap().is_none());
    assert_eq!(packages(&graph, REPO), 0);
}

#[test]
fn the_repositorys_own_lockfile_round_trips() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let lockfile = cargo_lock::Lockfile::load(root.join(FILE)).unwrap();
    let locked = |name: &str| {
        lockfile
            .packages
            .iter()
            .find(|package| package.name.as_str() == name)
            .unwrap_or_else(|| panic!("{name} is locked"))
            .version
            .to_string()
    };
    let graph = graph_for(REPO);
    let result = ingest(&graph, REPO, root).unwrap().expect("a lockfile");
    assert_eq!(result["versions"], json!(lockfile.packages.len()));
    assert_eq!(result["shadowed"], json!(0));

    // A registry package at its exact locked version.
    let serde = purl_at_version("serde", &locked("serde"));
    let entry = version(&graph, REPO, &serde);
    assert_eq!(entry["version"], json!(locked("serde")));
    assert_eq!(entry["source_kind"], json!("registry"));

    // A path dependency: no source line, so no source properties.
    let cypherlite = purl_at_version("cypherlite", &locked("cypherlite"));
    let entry = version(&graph, REPO, &cypherlite);
    assert_eq!(entry["version"], json!(locked("cypherlite")));
    assert_eq!(entry["source_kind"], json!("path"));
    assert_eq!(entry["source_url"], Value::Null);

    // The workspace roots this crate is built from are described.
    for name in ["graphicle", "graphicle-memory", "graphicle-runtime"] {
        let key = purl_at_version(name, &locked(name));
        assert_eq!(describes(&graph, REPO, &key), 1, "{name} is described");
    }

    // The resolved graph: this crate depends on serde.
    let runtime = purl_at_version("graphicle-runtime", &locked("graphicle-runtime"));
    assert_eq!(requires(&graph, REPO, &runtime, &serde), 1);
}

#[test]
fn a_lockfile_with_git_registry_and_path_sources_keeps_each_source() {
    let dir = folder(FIXTURE);
    let graph = graph_for(REPO);
    ingest(&graph, REPO, dir.path()).unwrap();

    let forked = version(&graph, REPO, "pkg:cargo/forked@0.3.0");
    assert_eq!(forked["source_kind"], json!("git"));
    assert!(
        forked["source_url"]
            .as_str()
            .is_some_and(|url| url.starts_with("https://github.com/example/forked")),
        "{forked}"
    );
    assert_eq!(
        forked["source_rev"],
        json!("0123456789abcdef0123456789abcdef01234567")
    );

    let serde = version(&graph, REPO, "pkg:cargo/serde@1.0.200");
    assert_eq!(serde["source_kind"], json!("registry"));
    assert!(
        serde["source_url"]
            .as_str()
            .is_some_and(|url| url.contains("crates.io-index")),
        "{serde}"
    );
    assert_eq!(serde["source_rev"], Value::Null);

    let local = version(&graph, REPO, "pkg:cargo/local@0.2.0");
    assert_eq!(local["source_kind"], json!("path"));
    assert_eq!(local["source_url"], Value::Null);
    assert_eq!(local["source_rev"], Value::Null);

    // The dependency edges reach all three kinds.
    assert_eq!(
        requires(
            &graph,
            REPO,
            "pkg:cargo/app@0.1.0",
            "pkg:cargo/forked@0.3.0"
        ),
        1
    );
    assert_eq!(
        requires(&graph, REPO, "pkg:cargo/app@0.1.0", "pkg:cargo/local@0.2.0"),
        1
    );
    assert_eq!(
        requires(
            &graph,
            REPO,
            "pkg:cargo/app@0.1.0",
            "pkg:cargo/serde@1.0.200"
        ),
        1
    );
}

#[test]
fn re_ingesting_an_unchanged_lockfile_writes_nothing_new() {
    let dir = folder(FIXTURE);
    let graph = graph_for(REPO);
    let first = ingest(&graph, REPO, dir.path()).unwrap().unwrap();
    let before = state(&graph);
    let second = ingest(&graph, REPO, dir.path()).unwrap().unwrap();
    assert_eq!(state(&graph), before);
    assert_eq!(first, second);
    assert_eq!(packages(&graph, REPO), 4);
    assert_eq!(first["versions"], json!(4));
    assert_eq!(first["described"], json!(4));
}

#[test]
fn a_re_lock_adds_versions_and_never_duplicates_packages() {
    let dir = folder(FIXTURE);
    let graph = graph_for(REPO);
    ingest(&graph, REPO, dir.path()).unwrap();
    std::fs::write(dir.path().join(FILE), RELOCKED).unwrap();
    ingest(&graph, REPO, dir.path()).unwrap();

    // New versions are new nodes; the new dependency is one new package.
    assert_eq!(packages(&graph, REPO), 5);
    assert_eq!(
        version(&graph, REPO, "pkg:cargo/serde@1.0.201")["version"],
        json!("1.0.201")
    );
    assert_eq!(
        version(&graph, REPO, "pkg:cargo/newdep@0.1.0")["version"],
        json!("0.1.0")
    );
    // The old version stays as a node, but the lock no longer describes it.
    assert_eq!(
        version(&graph, REPO, "pkg:cargo/serde@1.0.200")["version"],
        json!("1.0.200")
    );
    assert_eq!(describes(&graph, REPO, "pkg:cargo/serde@1.0.200"), 0);
    assert_eq!(describes(&graph, REPO, "pkg:cargo/serde@1.0.201"), 1);

    // One Package per name, one VERSION edge per locked version.
    let versions_of_serde = count(
        &graph,
        "MATCH (pk:Package {repository_id: $repo, purl: 'pkg:cargo/serde'})\
         -[:VERSION]->(pv:PackageVersion) RETURN count(*) AS n",
        json!({"repo": REPO}),
    );
    assert_eq!(versions_of_serde, 2);

    // The workspace member keeps its version while its dependencies change:
    // its REQUIRES follow the new lock, not the old one.
    let app = "pkg:cargo/app@0.1.0";
    assert_eq!(requires(&graph, REPO, app, "pkg:cargo/serde@1.0.201"), 1);
    assert_eq!(requires(&graph, REPO, app, "pkg:cargo/serde@1.0.200"), 0);
    assert_eq!(requires(&graph, REPO, app, "pkg:cargo/local@0.2.0"), 0);
    assert_eq!(requires(&graph, REPO, app, "pkg:cargo/newdep@0.1.0"), 1);
}

#[test]
fn forked_repositories_with_the_same_lockfile_never_cross_link() {
    let dir = folder(FIXTURE);
    let graph = graph_for(REPO);
    project(&graph, "repo-b");
    ingest(&graph, REPO, dir.path()).unwrap();
    ingest(&graph, "repo-b", dir.path()).unwrap();

    assert_eq!(packages(&graph, REPO), 4);
    assert_eq!(packages(&graph, "repo-b"), 4);
    let crossing = count(
        &graph,
        "MATCH (a)-[r]->(b) WHERE a.repository_id <> b.repository_id \
         RETURN count(*) AS n",
        json!({}),
    );
    assert_eq!(crossing, 0);
}

// ---------------------------------------------------------------------------
// The resolver (`resolve.rs`): the symbol seam of supply-chain plan §3.
// ---------------------------------------------------------------------------

/// One Rust import edge as the projection writes it: the file's symbol, the
/// imported name's symbol, and the edge's facts (`data` is the extractor
/// edge's attributes, minus its endpoints).
fn import(
    graph: &GraphStore,
    repository_id: &str,
    relpath: &str,
    symbol: &str,
    use_path: &str,
    local: bool,
    line: u64,
) {
    let mut data = json!({
        "relation": "imports_from", "context": "import", "confidence": "EXTRACTED",
        "source_file": relpath, "source_location": format!("L{line}"), "weight": 1.0,
        "_rust_use_path": use_path,
    });
    if local {
        data["_rust_use_local"] = json!(true);
    }
    graph
        .transaction(|db| {
            store_graph::write(
                db,
                "MERGE (f:Symbol {repository_id: $repo, stable_id: $file}) SET f.relpath = $path \
                 MERGE (s:Symbol {repository_id: $repo, stable_id: $symbol}) \
                 MERGE (f)-[r:IMPORTS_FROM {relpath: $path, source_location: $loc}]->(s) \
                 SET r.data = $data, r.relation = 'imports_from', r.valid_from_episode = 'e1'",
                p(
                    &json!({"repo": repository_id, "file": relpath, "path": relpath,
                    "symbol": symbol, "loc": format!("L{line}"),
                    "data": data.to_string()}),
                ),
            )?;
            Ok(())
        })
        .unwrap();
}

/// The real import facts of one of this crate's source files: the engine's
/// Rust extractor run on the file, each surviving `use` edge written as the
/// projection writes it (the group edges are dropped, as extract.rs's
/// `drop_rust_use_groups` drops them, and so is the marker it strips).
fn real_imports(graph: &GraphStore, repository_id: &str, relpath: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relpath);
    let extracted = graphicle::extractors::rust::extract_rust(&path);
    for edge in extracted["edges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["relation"] == "imports_from")
        // The plain edge of a grouped use is dropped downstream; the
        // binding edges carry the same paths.
        .filter(|e| e["_rust_use_group"] != true)
    {
        let mut attributes = edge.clone();
        let facts = attributes.as_object_mut().unwrap();
        let (source, target) = (facts.remove("source"), facts.remove("target"));
        let (source, target) = (source.unwrap(), target.unwrap());
        let relpath = attributes["source_file"].as_str().unwrap_or_default();
        let location = attributes["source_location"].as_str().unwrap_or_default();
        graph
            .transaction(|db| {
                store_graph::write(
                    db,
                    "MERGE (f:Symbol {repository_id: $repo, stable_id: $file}) SET f.relpath = $path \
                     MERGE (s:Symbol {repository_id: $repo, stable_id: $symbol}) \
                     MERGE (f)-[r:IMPORTS_FROM {relpath: $path, source_location: $loc}]->(s) \
                     SET r.data = $data, r.relation = 'imports_from', r.valid_from_episode = 'e1'",
                    p(&json!({"repo": repository_id,
                        "file": source.as_str().unwrap_or_default(),
                        "path": relpath, "symbol": target.as_str().unwrap_or_default(),
                        "loc": location, "data": attributes.to_string()})),
                )?;
                Ok(())
            })
            .unwrap();
    }
}

/// How many of `repository_id`'s symbols resolve to `purl`.
fn resolves_to(graph: &GraphStore, repository_id: &str, purl: &str) -> i64 {
    count(
        graph,
        "MATCH (s:Symbol {repository_id: $repo})\
         -[:RESOLVES_TO]->(pk:Package {repository_id: $repo, purl: $purl}) \
         RETURN count(*) AS n",
        json!({"repo": repository_id, "purl": purl}),
    )
}

/// The `UnresolvedPackage` node of `package` in `repository_id`, or `Null`.
fn unresolved(graph: &GraphStore, repository_id: &str, package: &str) -> Value {
    row(
        graph,
        "MATCH (u:UnresolvedPackage {repository_id: $repo, package: $package}) \
         RETURN u.symbol AS symbol, u.use_path AS use_path, u.relpath AS relpath, \
                u.line AS line, u.reason AS reason",
        json!({"repo": repository_id, "package": package}),
    )
}

#[test]
fn real_imports_of_this_repository_resolve_to_their_locked_packages() {
    // The rule of supply-chain plan §3, end to end on this crate's own
    // facts: the real extractor reads real `use` statements, the real
    // lockfile names the packages, and a use path's first segment resolves
    // the imported-name symbol to the locked `:Package`.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let graph = graph_for(REPO);
    ingest(&graph, REPO, root).unwrap().expect("a lockfile");
    for relpath in ["src/api.rs", "src/ingestion.rs", "src/supply_chain/lock.rs"] {
        real_imports(&graph, REPO, relpath);
    }
    let report = resolve::resolve(&graph, REPO).unwrap().expect("resolved");
    // The considered imports all landed somewhere: none dropped silently.
    let bucket = |key: &str| report[key].as_i64().unwrap_or_default();
    assert_eq!(
        bucket("imports"),
        bucket("resolved") + bucket("local") + bucket("stdlib") + bucket("unresolved"),
        "{report}"
    );
    assert!(bucket("resolved") >= 6, "{report}");
    // Every crate these files name is locked, and their hyphenated spellings
    // (`cargo_lock` is `cargo-lock`) and member names (`graphicle_memory`)
    // are not unresolved imports.
    assert_eq!(bucket("unresolved"), 0, "{report}");

    // The locked crate each use names: `serde` (api.rs), `tokio`
    // (ingestion.rs), `serde_json` and `cargo_lock` (lock.rs) — and the
    // package carries the exact locked version.
    let lockfile = cargo_lock::Lockfile::load(root.join(FILE)).unwrap();
    let locked = |name: &str| {
        lockfile
            .packages
            .iter()
            .find(|package| package.name.as_str() == name)
            .unwrap_or_else(|| panic!("{name} is locked"))
            .version
            .to_string()
    };
    for name in ["serde", "tokio", "serde_json", "cargo-lock", "axum"] {
        let purl = lock::purl(name);
        assert!(
            resolves_to(&graph, REPO, &purl) >= 1,
            "{name} resolves: {report}"
        );
        let at_version = count(
            &graph,
            "MATCH (pk:Package {repository_id: $repo, purl: $purl})-[:VERSION]->\
             (pv:PackageVersion {repository_id: $repo, purl_at_version: $key}) \
             RETURN count(*) AS n",
            json!({"repo": REPO, "purl": purl, "key": purl_at_version(name, &locked(name))}),
        );
        assert_eq!(at_version, 1, "{name} at its locked version");
    }
    // This crate's own paths and the standard library resolve to nothing.
    for name in ["graphicle-memory", "cypherlite"] {
        assert_eq!(resolves_to(&graph, REPO, &lock::purl(name)), 0);
    }
}

#[test]
fn an_import_matching_nothing_is_negative_evidence_not_a_silent_drop() {
    let dir = folder(FIXTURE);
    let graph = graph_for(REPO);
    ingest(&graph, REPO, dir.path()).unwrap();
    // One import of each kind the rule distinguishes: a locked registry
    // crate, a locked git crate, this crate's own paths (a `crate` head, a
    // head the extractor marked local, two workspace-member heads), the
    // standard library, and a crate the lockfile has no package for.
    import(
        &graph,
        REPO,
        "src/main.rs",
        "Deserialize",
        "serde::Deserialize",
        false,
        3,
    );
    import(
        &graph,
        REPO,
        "src/main.rs",
        "Thing",
        "forked::Thing",
        false,
        4,
    );
    import(
        &graph,
        REPO,
        "src/main.rs",
        "Ghost",
        "ghost::Thing",
        false,
        5,
    );
    import(
        &graph,
        REPO,
        "src/main.rs",
        "Own",
        "crate::own::Own",
        false,
        6,
    );
    import(
        &graph,
        REPO,
        "src/main.rs",
        "Marked",
        "modnamed::Marked",
        true,
        7,
    );
    import(
        &graph,
        REPO,
        "src/main.rs",
        "Helper",
        "local::helper",
        false,
        8,
    );
    import(&graph, REPO, "src/main.rs", "Main", "app::main", false, 9);
    import(
        &graph,
        REPO,
        "src/main.rs",
        "File",
        "std::fs::File",
        false,
        10,
    );

    let report = resolve::resolve(&graph, REPO).unwrap().expect("resolved");
    assert_eq!(
        report,
        json!({
            "imports": 8, "resolved": 2, "local": 4, "stdlib": 1, "unresolved": 1,
            "unreadable": 0, "unresolved_packages": 1, "edges": 2,
        }),
        "{report}"
    );
    // The unresolved import is a fact with its reason, not a gap: the
    // UnresolvedCross discipline.
    let ghost = unresolved(&graph, REPO, "ghost");
    assert_eq!(ghost["symbol"], json!("Ghost"));
    assert_eq!(ghost["use_path"], json!("ghost::Thing"));
    assert_eq!(ghost["relpath"], json!("src/main.rs"));
    assert_eq!(ghost["line"], json!(5));
    assert_eq!(
        ghost["reason"],
        json!("no locked crate named 'ghost' and no workspace member of that name")
    );
    // What resolved is a name-level package edge; what did not, is not one.
    assert_eq!(resolves_to(&graph, REPO, "pkg:cargo/serde"), 1);
    assert_eq!(resolves_to(&graph, REPO, "pkg:cargo/forked"), 1);
    assert_eq!(resolves_to(&graph, REPO, "pkg:cargo/ghost"), 0);
    // Local paths, members and the standard library leave no trace: they
    // are not negative evidence.
    assert_eq!(
        count(
            &graph,
            "MATCH (u:UnresolvedPackage {repository_id: $repo}) RETURN count(*) AS n",
            json!({"repo": REPO}),
        ),
        1
    );
}

#[test]
fn resolution_is_recomputed_never_accumulated() {
    let dir = folder(FIXTURE);
    let graph = graph_for(REPO);
    ingest(&graph, REPO, dir.path()).unwrap();
    import(
        &graph,
        REPO,
        "src/main.rs",
        "Deserialize",
        "serde::Deserialize",
        false,
        3,
    );
    import(
        &graph,
        REPO,
        "src/main.rs",
        "Newdep",
        "newdep::Thing",
        false,
        4,
    );
    let first = resolve::resolve(&graph, REPO).unwrap().unwrap();
    assert_eq!(first["unresolved"], json!(1));
    assert!(unresolved(&graph, REPO, "newdep")["reason"].is_string());

    // Re-running on unchanged facts writes the same state.
    let before = state(&graph);
    let second = resolve::resolve(&graph, REPO).unwrap().unwrap();
    assert_eq!(state(&graph), before);
    assert_eq!(first, second);

    // A re-lock that adds the crate turns the negative evidence around:
    // the recomputed resolution keeps no stale node.
    std::fs::write(dir.path().join(FILE), RELOCKED).unwrap();
    ingest(&graph, REPO, dir.path()).unwrap();
    let third = resolve::resolve(&graph, REPO).unwrap().unwrap();
    assert_eq!(third["unresolved"], json!(0));
    assert_eq!(third["unresolved_packages"], json!(0));
    assert_eq!(resolves_to(&graph, REPO, "pkg:cargo/newdep"), 1);
    assert_eq!(unresolved(&graph, REPO, "newdep"), Value::Null);
}

#[test]
fn forked_repositories_resolve_within_their_own_project() {
    // The keys are the symbol's stable_id and the package's purl under each
    // repository_id, so the same imports in two forks resolve twice, once
    // per project, and never across.
    let dir = folder(FIXTURE);
    let graph = graph_for(REPO);
    project(&graph, "repo-b");
    for repo in [REPO, "repo-b"] {
        ingest(&graph, repo, dir.path()).unwrap();
        import(
            &graph,
            repo,
            "src/main.rs",
            "Deserialize",
            "serde::Deserialize",
            false,
            3,
        );
        resolve::resolve(&graph, repo).unwrap().expect("resolved");
    }
    assert_eq!(resolves_to(&graph, REPO, "pkg:cargo/serde"), 1);
    assert_eq!(resolves_to(&graph, "repo-b", "pkg:cargo/serde"), 1);
    let crossing = count(
        &graph,
        "MATCH (a)-[r:RESOLVES_TO]->(b) WHERE a.repository_id <> b.repository_id \
         RETURN count(*) AS n",
        json!({}),
    );
    assert_eq!(crossing, 0);
}

#[test]
fn a_project_without_lockfile_packages_resolves_nothing() {
    let graph = graph_for(REPO);
    import(
        &graph,
        REPO,
        "src/main.rs",
        "Deserialize",
        "serde::Deserialize",
        false,
        3,
    );
    // Nothing to resolve against: no report, and nothing written.
    assert!(resolve::resolve(&graph, REPO).unwrap().is_none());
    assert_eq!(
        count(
            &graph,
            "MATCH (:Symbol {repository_id: $repo})-[r:RESOLVES_TO]->() RETURN count(*) AS n",
            json!({"repo": REPO}),
        ),
        0
    );
    assert_eq!(
        count(
            &graph,
            "MATCH (u:UnresolvedPackage {repository_id: $repo}) RETURN count(*) AS n",
            json!({"repo": REPO}),
        ),
        0
    );
}
