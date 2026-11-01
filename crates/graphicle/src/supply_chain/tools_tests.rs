//! The supply-chain tools, driven through the tool surface (`api::mcp`, the
//! `tools/call` path the server answers) on an in-memory-style store: a
//! project whose lockfile locks `time` 0.2.22 (affected by RUSTSEC-2020-0071)
//! and a clean `serde`, with a file that imports `time` on line 3. The OSV
//! and deps.dev answers are the golden fixtures; no test reaches the network.

use super::lock;
use super::on_demand::{self, Fact};
use super::osv::{self, Client};
use super::osv_tests::LOCK;
use super::resolve;
use super::tools::{dependencies_by_file, dependency_lines};
use crate::api::{self, App};
use crate::model;
use crate::store::{Store, s};
use crate::store_graph::{self, GraphStore};
use crate::supply_chain::deps_dev::BASE_URL;
use crate::supply_chain::http_cache::HttpCache;
use crate::supply_chain::http_cache::test_transport::{FixtureTransport, json as body};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

const QUERYBATCH: &str =
    include_str!("../../tests/fixtures/supply_chain/osv/querybatch_time_serde.json");
const RUSTSEC: &str =
    include_str!("../../tests/fixtures/supply_chain/osv/vuln_rustsec_2020_0071.json");
const GHSA: &str =
    include_str!("../../tests/fixtures/supply_chain/osv/vuln_ghsa_wcg3_cvx6_7396.json");
const REQUIREMENTS: &str =
    include_str!("../../tests/fixtures/supply_chain/deps_dev/requirements_cargo_time_0_2_22.json");

const APP: &str = "src/app.rs";
const TIME_KEY: &str = "pkg:cargo/time@0.2.22";
const SERDE_KEY: &str = "pkg:cargo/serde@1.0.200";
const KNOWN_AT: &str = "2026-11-01T04:00:00Z";
const AS_OF: &str = "2026-10-01T00:00:00Z";
const RUSTSEC_ID: &str = "RUSTSEC-2020-0071";

/// A project with the lockfile, the import facts and the OSV scan written.
struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
    app: App,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let created = store.create_workspace("Supply chain").unwrap();
        let workspace = s(&created, "id").to_string();
        let key = s(&created, "capability").to_string();
        let registered = store
            .register(
                &workspace,
                &key,
                &json!({"project_key": "app", "name": "app"}),
            )
            .unwrap();
        let repo = s(&registered, "id").to_string();
        let graph = store.graph();
        lock::write(&graph, &repo, &lock::parse(LOCK).unwrap()).unwrap();
        // `use time::OffsetDateTime;` on line 3 of app.rs, and `use serde::Serialize;` on line 1.
        import(&graph, &repo, APP, 3, "time::OffsetDateTime");
        import(&graph, &repo, APP, 1, "serde::Serialize");
        resolve::resolve(&graph, &repo).unwrap().unwrap();
        scan(&graph, &repo);
        let app = App::new(store.clone(), model::Config::default(), vec![]).unwrap();
        Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo,
            app,
        }
    }

    fn graph(&self) -> Arc<GraphStore> {
        self.store.graph()
    }

    /// One `tools/call` of `name`, as the server answers it: the reply's
    /// `structuredContent`, or the message of the error it carries.
    fn call(&self, name: &str, arguments: Value) -> Result<Value, String> {
        let reply = api::mcp(
            &self.app,
            &self.workspace,
            &self.key,
            &self.repo,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":name,"arguments":arguments}}),
        )
        .unwrap();
        if let Some(error) = reply.get("error") {
            return Err(error["message"].as_str().unwrap_or_default().to_owned());
        }
        let result = &reply["result"];
        if result["isError"] == true {
            return Err(result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned());
        }
        Ok(result["structuredContent"].clone())
    }
}

/// One Rust import as the projection writes it: the file's symbol, the
/// imported name's symbol, and the edge's facts (resolve.rs reads them).
fn import(graph: &GraphStore, repository: &str, path: &str, line: u64, use_path: &str) {
    let symbol = use_path.to_owned();
    let data = json!({
        "relation": "imports_from", "context": "import", "confidence": "EXTRACTED",
        "source_file": path, "source_location": format!("L{line}"), "weight": 1.0,
        "_rust_use_path": use_path,
    });
    graph
        .transaction(|db| {
            store_graph::write(
                db,
                "MERGE (f:Symbol {repository_id: $repo, stable_id: $file}) SET f.relpath = $path \
                 MERGE (s:Symbol {repository_id: $repo, stable_id: $symbol}) \
                 MERGE (f)-[r:IMPORTS_FROM {relpath: $path, source_location: $loc}]->(s) \
                 SET r.data = $data, r.relation = 'imports_from', r.valid_from_episode = 'e1'",
                store_graph::params(&json!({"repo": repository, "file": path, "path": path,
                    "symbol": symbol, "loc": format!("L{line}"), "data": data.to_string()})),
            )?;
            Ok(())
        })
        .unwrap();
}

/// OSV's answers for the lockfile, from the golden fixtures, written as the
/// sync writes them.
fn scan(graph: &GraphStore, repository: &str) {
    let transport = Arc::new(FixtureTransport::new([
        (format!("{}/querybatch", osv::BASE_URL), body(QUERYBATCH)),
        (
            format!("{}/vulns/RUSTSEC-2020-0071", osv::BASE_URL),
            body(RUSTSEC),
        ),
        (
            format!("{}/vulns/GHSA-wcg3-cvx6-7396", osv::BASE_URL),
            body(GHSA),
        ),
    ]));
    let cache_dir = TempDir::new().unwrap();
    let cache = HttpCache::for_store(cache_dir.path(), osv::NAMESPACE, Duration::ZERO);
    let targets = osv::targets(graph, repository).unwrap();
    let scan = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(osv::scan(&Client::new(transport, cache), &targets))
        .unwrap();
    osv::write(graph, repository, &scan, KNOWN_AT).unwrap();
}

/// The `name`s of the tools the server lists.
fn listed(fixture: &Fixture) -> Vec<String> {
    let reply = api::mcp(
        &fixture.app,
        &fixture.workspace,
        &fixture.key,
        &fixture.repo,
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .unwrap();
    reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}

/// The `files` entry of `path` in an answer.
fn file<'a>(answer: &'a Value, path: &str) -> &'a Value {
    answer["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == path)
        .unwrap_or_else(|| panic!("no file {path} in {answer}"))
}

#[test]
fn the_three_tools_are_listed_and_answer_through_the_tool_surface() {
    let fixture = Fixture::new();
    let names = listed(&fixture);
    for tool in ["deps_of", "vuln_impact", "license_of"] {
        assert!(
            names.contains(&tool.to_owned()),
            "{tool} is not listed: {names:?}"
        );
    }
    // Each runs through the same admission and metering as the other tools:
    // a read, answered with its structured content. deps.dev is not reached.
    on_demand::install_refused();
    for (name, arguments) in [
        ("deps_of", json!({"file": APP})),
        ("vuln_impact", json!({"advisory": RUSTSEC_ID})),
        ("license_of", json!({"package": "serde"})),
    ] {
        let answer = fixture
            .call(name, arguments)
            .unwrap_or_else(|error| panic!("{name} failed: {error}"));
        assert!(answer["summary"].is_object(), "{name}: {answer}");
    }
}

#[test]
fn a_call_must_name_exactly_one_field() {
    let fixture = Fixture::new();
    let error = fixture.call("deps_of", json!({})).unwrap_err();
    assert!(
        error.contains("Name exactly one of symbol, file"),
        "{error}"
    );
    let error = fixture
        .call(
            "vuln_impact",
            json!({"advisory": RUSTSEC_ID, "package": "time"}),
        )
        .unwrap_err();
    assert!(
        error.contains("Name exactly one of advisory, package"),
        "{error}"
    );
}

#[test]
fn vuln_impact_on_an_advisory_cites_the_import_line_of_the_using_file() {
    let fixture = Fixture::new();
    let answer = fixture
        .call("vuln_impact", json!({"advisory": RUSTSEC_ID}))
        .unwrap();
    // The advisory, then the locked version it affects, then the import of
    // that version's package: the citation is the import edge's path:line.
    assert_eq!(answer["advisory"]["osv_id"], RUSTSEC_ID);
    assert_eq!(answer["versions"][0]["purl_at_version"], TIME_KEY);
    assert_eq!(answer["versions"][0]["status"], "affected");
    assert_eq!(file(&answer, APP)["citation"], "src/app.rs:3");
    assert_eq!(file(&answer, APP)["lines"], json!([3]));
    assert_eq!(answer["summary"]["files"], 1);
    assert_eq!(answer["summary"]["citations"], 1);
    assert_eq!(answer["summary"]["affected"], 1);
    assert_eq!(answer["not_known"], json!([]));
}

#[test]
fn vuln_impact_on_a_package_names_its_advisories_or_its_clean_scan() {
    let fixture = Fixture::new();
    let time = fixture
        .call("vuln_impact", json!({"package": "time"}))
        .unwrap();
    assert_eq!(time["versions"][0]["status"], "affected");
    let advisories = time["versions"][0]["advisories"].as_array().unwrap();
    assert!(
        advisories.iter().any(|a| a["osv_id"] == RUSTSEC_ID),
        "{time}"
    );
    assert_eq!(file(&time, APP)["citation"], "src/app.rs:3");
    // serde has no advisory: the clean scan answers, with its date and source.
    let serde = fixture
        .call("vuln_impact", json!({"package": "serde"}))
        .unwrap();
    assert_eq!(serde["versions"][0]["purl_at_version"], SERDE_KEY);
    assert_eq!(serde["versions"][0]["status"], "clean");
    assert_eq!(serde["versions"][0]["source"], "osv");
    assert_eq!(serde["versions"][0]["as_of"], KNOWN_AT);
    assert_eq!(file(&serde, APP)["citation"], "src/app.rs:1");
}

#[test]
fn deps_of_a_file_lists_its_packages_with_import_lines_and_lockfile_edges() {
    let fixture = Fixture::new();
    // deps.dev is not reached: the requirements are named as not known.
    on_demand::install_refused();
    let answer = fixture.call("deps_of", json!({"file": APP})).unwrap();
    let packages = answer["packages"].as_array().unwrap();
    assert_eq!(packages.len(), 2, "{answer}");
    let time = packages.iter().find(|p| p["name"] == "time").unwrap();
    assert_eq!(time["cited"], json!(["src/app.rs:3"]));
    assert_eq!(time["versions"][0]["version"], "0.2.22");
    assert_eq!(time["versions"][0]["requires"], json!([]));
    assert_eq!(time["versions"][0]["requirements"], Value::Null);
    assert_eq!(answer["summary"]["files"], 1);
    assert_eq!(answer["summary"]["citations"], 2);
    let not_known = answer["not_known"].as_array().unwrap();
    assert!(
        not_known.iter().any(|note| note.as_str().unwrap().contains(
            "requirements of pkg:cargo/time@0.2.22 are not known: deps.dev was not reached"
        )),
        "{answer}"
    );
}

#[test]
fn deps_of_takes_the_requirements_from_the_read_through_layer_on_a_miss() {
    let fixture = Fixture::new();
    let routes = [(
        format!("{BASE_URL}/systems/CARGO/packages/time/versions/0.2.22:requirements"),
        body(REQUIREMENTS),
    )];
    on_demand::install_fixture(Arc::new(FixtureTransport::new(routes)));
    let answer = fixture
        .call("deps_of", json!({"symbol": "time::OffsetDateTime"}))
        .unwrap();
    let time = &answer["packages"][0];
    assert_eq!(time["name"], "time");
    let requirements = time["versions"][0]["requirements"].as_array().unwrap();
    assert!(!requirements.is_empty(), "{answer}");
    assert_eq!(requirements[0]["kind"], "normal");
}

#[test]
fn license_of_answers_from_a_pin_and_fetches_nothing() {
    let fixture = Fixture::new();
    on_demand::pin(
        &fixture.graph(),
        &fixture.repo,
        &[Fact::Licenses {
            purl_at_version: TIME_KEY.to_owned(),
            expressions: vec!["MIT OR Apache-2.0".to_owned()],
        }],
        "analysis-1",
        KNOWN_AT,
        AS_OF,
    )
    .unwrap();
    // A wire that panics on any request: the pin must answer alone.
    let wire = on_demand::install_panic();
    let answer = fixture
        .call("license_of", json!({"package": "time"}))
        .unwrap();
    let license = &answer["licenses"][0];
    assert_eq!(license["source"], "pinned");
    assert_eq!(license["expressions"], json!(["MIT OR Apache-2.0"]));
    assert_eq!(license["as_of"], AS_OF);
    assert_eq!(answer["summary"]["pinned"], 1);
    assert_eq!(wire.calls(), 0);
}

#[test]
fn a_store_with_no_network_and_no_pins_answers_from_local_facts_and_names_what_is_not_known() {
    let fixture = Fixture::new();
    on_demand::install_refused();
    // deps_of and vuln_impact need no network for their local facts.
    let deps = fixture.call("deps_of", json!({"file": APP})).unwrap();
    assert_eq!(deps["summary"]["packages"], 2);
    assert_eq!(file(&deps, APP)["citation"], "src/app.rs:1");
    assert_eq!(file(&deps, APP)["lines"], json!([1, 3]));
    let impact = fixture
        .call("vuln_impact", json!({"advisory": RUSTSEC_ID}))
        .unwrap();
    assert_eq!(file(&impact, APP)["citation"], "src/app.rs:3");
    // license_of has no local license: each version is named as not known.
    let license = fixture
        .call("license_of", json!({"symbol": "time::OffsetDateTime"}))
        .unwrap();
    assert_eq!(license["licenses"][0]["source"], "unknown");
    assert_eq!(license["licenses"][0]["expressions"], json!([]));
    assert_eq!(license["summary"]["unknown"], 1);
    let not_known = license["not_known"][0].as_str().unwrap();
    assert!(
        not_known.contains("the license of pkg:cargo/time@0.2.22 is not known")
            && not_known.contains("deps.dev was not reached"),
        "{license}"
    );
    // A name the store does not hold is an answer that says so, not an error.
    let unknown = fixture
        .call("vuln_impact", json!({"package": "no-such-crate"}))
        .unwrap();
    assert_eq!(unknown["versions"], json!([]));
    assert!(
        unknown["not_known"][0]
            .as_str()
            .unwrap()
            .contains("no-such-crate")
    );
    let absent = fixture
        .call("deps_of", json!({"symbol": "nothing_here"}))
        .unwrap();
    assert_eq!(absent["packages"], json!([]));
    assert!(
        absent["not_known"][0]
            .as_str()
            .unwrap()
            .contains("nothing_here")
    );
}

#[test]
fn the_pack_facts_resolve_each_changed_file_to_its_packages_and_their_advisories() {
    let fixture = Fixture::new();
    let by_file = dependencies_by_file(&fixture.graph(), &fixture.repo).unwrap();
    let dependencies = &by_file[APP];
    let time = dependencies.iter().find(|d| d.name == "time").unwrap();
    assert_eq!(time.line, Some(3));
    assert_eq!(time.versions[0].version, "0.2.22");
    assert!(time.versions[0].advisories.contains(&RUSTSEC_ID.to_owned()));
    let lines = dependency_lines(APP, dependencies);
    assert!(
        lines.contains("- time 0.2.22, imported at src/app.rs:3; requires: none"),
        "{lines}"
    );
    assert!(lines.contains(RUSTSEC_ID), "{lines}");
    // A file with no import that resolves names no package.
    assert!(!by_file.contains_key("src/other.rs"));
}
