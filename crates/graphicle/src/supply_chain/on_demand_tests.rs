//! The on-the-fly layer's tests: the read-through surface over fixture
//! transports (the golden deps.dev answers under
//! `tests/fixtures/supply_chain/deps_dev/`), and the pin write path over
//! in-memory stores holding a lockfile's package facts. The offline
//! guarantee is proven by a stub that panics on any request: a full sync of
//! a lockfile-carrying project runs with it installed and stays silent. No
//! test reaches the network.

use super::on_demand::{
    Fact, OnDemand, Requirement, license_answer, pin, pinned_deprecation, pinned_licenses,
    pinned_requirements, pinned_scorecard, pinned_source_repo, project_id,
};
use crate::store::s;
use crate::store_graph::{self, GraphStore};
use crate::supply_chain::deps_dev::{self, BASE_URL};
use crate::supply_chain::http_cache::HttpCache;
use crate::supply_chain::http_cache::test_transport::{FixtureTransport, json};
use crate::supply_chain::lock;
use crate::supply_chain::on_demand;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const VERSION: &str =
    include_str!("../../tests/fixtures/supply_chain/deps_dev/version_cargo_time_0_2_22.json");
const PACKAGE: &str =
    include_str!("../../tests/fixtures/supply_chain/deps_dev/package_cargo_time.json");
const REQUIREMENTS: &str =
    include_str!("../../tests/fixtures/supply_chain/deps_dev/requirements_cargo_time_0_2_22.json");
const PROJECT: &str =
    include_str!("../../tests/fixtures/supply_chain/deps_dev/project_github_time.json");

const REPO: &str = "repo-a";
const KNOWN_AT: &str = "2026-11-01T04:00:00Z";
const AS_OF: &str = "2026-11-01T05:00:00Z";
const TIME_KEY: &str = "pkg:cargo/time@0.2.22";
const SERDE_KEY: &str = "pkg:cargo/serde@1.0.200";

/// The same version in a changed world: the license is Apache-2.0 alone.
const CHANGED_TIME: &str = r#"{"versionKey": {"system": "CARGO", "name": "time",
    "version": "0.2.22"}, "licenses": ["Apache-2.0"]}"#;

/// serde's entry in that changed world, for a version no pin covers.
const CHANGED_SERDE: &str = r#"{"versionKey": {"system": "CARGO", "name": "serde",
    "version": "1.0.200"}, "licenses": ["Apache-2.0"]}"#;

/// The project's lockfile, with `time`'s own dependencies locked: the
/// manifest requirements join onto the `REQUIRES` edges these write.
pub(crate) const LOCK_WITH_DEPS: &str = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "serde",
 "time",
]

[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"

[[package]]
name = "time"
version = "0.2.22"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = [
 "const_fn",
 "libc",
]
checksum = "1111111111111111111111111111111111111111111111111111111111111111"

[[package]]
name = "const_fn"
version = "0.4.9"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "2222222222222222222222222222222222222222222222222222222222222222"

[[package]]
name = "libc"
version = "0.2.155"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "3333333333333333333333333333333333333333333333333333333333333333"
"#;

/// The world as the golden fixtures hold it: the four entries the surface
/// can ask for (the version, the package, the manifest, the project).
fn golden_routes() -> Vec<(String, &'static str)> {
    let version = version_url("time", "0.2.22");
    vec![
        (version.clone(), VERSION),
        (package_url("time"), PACKAGE),
        (format!("{version}:requirements"), REQUIREMENTS),
        (project_url("github.com/time-rs/time"), PROJECT),
    ]
}

/// The world after it changed: both versions' licenses are Apache-2.0.
fn changed_routes() -> Vec<(String, &'static str)> {
    vec![
        (version_url("time", "0.2.22"), CHANGED_TIME),
        (version_url("serde", "1.0.200"), CHANGED_SERDE),
    ]
}

fn base() -> String {
    BASE_URL.to_owned()
}

fn version_url(name: &str, version: &str) -> String {
    format!(
        "{}/systems/CARGO/packages/{name}/versions/{version}",
        base()
    )
}

fn package_url(name: &str) -> String {
    format!("{}/systems/CARGO/packages/{name}", base())
}

fn project_url(project: &str) -> String {
    format!("{}/projects/{}", base(), project.replace('/', "%2F"))
}

/// A read-through layer over `routes`, its cache in `store_dir` and its
/// network calls gated `interval` apart. The returned transport counts the
/// requests that reached it.
fn layer_over(
    store_dir: &Path,
    routes: Vec<(String, &str)>,
    interval: Duration,
) -> (OnDemand<Arc<FixtureTransport>>, Arc<FixtureTransport>) {
    let transport = Arc::new(FixtureTransport::new(
        routes.into_iter().map(|(url, body)| (url, json(body))),
    ));
    let cache = HttpCache::for_store(store_dir, deps_dev::NAMESPACE, interval);
    (OnDemand::new(transport.clone(), cache), transport)
}

/// An in-memory store holding the lockfile's package facts for `REPO`.
fn graph_with_lock() -> GraphStore {
    let graph = GraphStore::open_in_memory().unwrap();
    lock::write(&graph, REPO, &lock::parse(LOCK_WITH_DEPS).unwrap()).unwrap();
    graph
}

/// The first row of a query over the graph, or `Null`.
fn row(graph: &GraphStore, cypher: &str, params: Value) -> Value {
    graph
        .read(|db| store_graph::one(db, cypher, store_graph::params(&params)))
        .unwrap()
        .unwrap_or(Value::Null)
}

/// The rows of a query over the graph, in `REPO`'s scope.
fn rows(graph: &GraphStore, cypher: &str, params: Value) -> Vec<Value> {
    graph
        .read(|db| store_graph::rows(db, cypher, store_graph::params(&params)))
        .unwrap()
}

fn licenses(key: &str, expressions: &[&str]) -> Fact {
    Fact::Licenses {
        purl_at_version: key.to_owned(),
        expressions: expressions.iter().map(|e| e.to_string()).collect(),
    }
}

fn requirement(
    name: &str,
    requirement: &str,
    kind: &str,
    optional: bool,
    features: &[&str],
    target: &str,
) -> Requirement {
    Requirement {
        name: name.to_owned(),
        requirement: requirement.to_owned(),
        kind: kind.to_owned(),
        optional,
        features: features.iter().map(|f| f.to_string()).collect(),
        target: target.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// The read-through surface: fetched only when asked, only the entry asked
// for, through the shared cache and throttle.
// ---------------------------------------------------------------------------

#[test]
fn project_ids_come_from_repository_urls() {
    assert_eq!(
        project_id("https://github.com/time-rs/time"),
        "github.com/time-rs/time"
    );
    assert_eq!(
        project_id("http://github.com/time-rs/time/"),
        "github.com/time-rs/time"
    );
    assert_eq!(
        project_id("github.com/time-rs/time"),
        "github.com/time-rs/time"
    );
}

#[tokio::test]
async fn every_call_fetches_only_the_entry_it_was_asked_for() {
    let world = tempfile::tempdir().unwrap();
    let (layer, wire) = layer_over(world.path(), golden_routes(), Duration::ZERO);

    assert_eq!(
        layer.licenses_for(TIME_KEY).await.unwrap(),
        Some(licenses(TIME_KEY, &["MIT OR Apache-2.0"]))
    );
    assert_eq!(
        layer.deprecation_for(TIME_KEY).await.unwrap(),
        Some(Fact::Deprecation {
            purl_at_version: TIME_KEY.into(),
            deprecated: false,
            reason: None,
        })
    );
    // The source repository comes from the version's own entry, already
    // cached by the license question: no second fetch.
    assert_eq!(
        layer.source_repo_for(TIME_KEY).await.unwrap(),
        Some(Fact::SourceRepo {
            purl_at_version: TIME_KEY.into(),
            url: "https://github.com/time-rs/time".into(),
        })
    );
    assert_eq!(
        layer.requirements_for(TIME_KEY).await.unwrap(),
        Some(Fact::Requirements {
            purl_at_version: TIME_KEY.into(),
            dependencies: vec![
                requirement("const_fn", "^0.4.2", "normal", false, &[], ""),
                requirement("libc", "^0.2", "normal", true, &[], "cfg(unix)"),
                requirement("rand", "^0.7", "dev", false, &[], ""),
                requirement(
                    "winapi",
                    "^0.3",
                    "normal",
                    true,
                    &["minwinbase", "minwindef", "timezoneapi"],
                    "cfg(windows)"
                ),
            ],
        })
    );
    assert_eq!(
        layer
            .scorecard_for(TIME_KEY, "github.com/time-rs/time")
            .await
            .unwrap(),
        Some(Fact::Scorecard {
            purl_at_version: TIME_KEY.into(),
            project: "github.com/time-rs/time".into(),
            stars: 1347,
            overall: 4.6,
            scored_at: Some("2026-08-24T00:00:00+00:00".into()),
        })
    );
    // Four entries asked for, four requests: the version, the package, the
    // manifest and the project — never a version list, never a dependency
    // graph, nothing ecosystem-wide.
    assert_eq!(wire.calls(), 4);
    // The same questions again are cache hits.
    layer.licenses_for(TIME_KEY).await.unwrap();
    layer.requirements_for(TIME_KEY).await.unwrap();
    assert_eq!(wire.calls(), 4);
}

#[tokio::test]
async fn an_unknown_entry_is_no_fact_and_a_bad_purl_is_an_error() {
    let world = tempfile::tempdir().unwrap();
    let (layer, _) = layer_over(world.path(), golden_routes(), Duration::ZERO);
    // deps.dev has no entry: no fact, not an error.
    assert_eq!(
        layer.licenses_for("pkg:cargo/ghost@1.0.0").await.unwrap(),
        None
    );
    assert_eq!(
        layer
            .requirements_for("pkg:cargo/ghost@1.0.0")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        layer
            .deprecation_for("pkg:cargo/ghost@1.0.0")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        layer
            .scorecard_for(TIME_KEY, "github.com/no/such")
            .await
            .unwrap(),
        None
    );
    // A purl that is not a versioned purl is a caller error.
    let error = layer.licenses_for("serde@1.0.200").await.unwrap_err();
    assert!(error.message.contains("purl"), "{error}");
    assert!(layer.licenses_for("pkg:cargo/serde").await.is_err());
}

#[tokio::test]
async fn a_version_with_no_known_license_is_noassertion() {
    let world = tempfile::tempdir().unwrap();
    let (layer, _) = layer_over(
        world.path(),
        vec![(
            version_url("serde", "1.0.200"),
            r#"{"versionKey": {"system": "CARGO", "name": "serde",
                "version": "1.0.200"}, "licenses": []}"#,
        )],
        Duration::ZERO,
    );
    // The negative answer is a citable fact (the SCANNED_CLEAN discipline),
    // so a pin of it has an edge to write and the answer is re-runnable.
    assert_eq!(
        layer.licenses_for(SERDE_KEY).await.unwrap(),
        Some(licenses(SERDE_KEY, &["NOASSERTION"]))
    );
}

#[tokio::test]
async fn a_deprecated_version_carries_its_reason() {
    let world = tempfile::tempdir().unwrap();
    let (layer, _) = layer_over(world.path(), golden_routes(), Duration::ZERO);
    // The reason lives only in the package's version list (deps_dev.rs),
    // which is why the deprecation question is the package's.
    assert_eq!(
        layer
            .deprecation_for("pkg:cargo/time@0.1.45")
            .await
            .unwrap(),
        Some(Fact::Deprecation {
            purl_at_version: "pkg:cargo/time@0.1.45".into(),
            deprecated: true,
            reason: Some("Superseded by time 0.3".into()),
        })
    );
}

#[tokio::test]
async fn on_demand_fetches_are_throttled_through_the_shared_layer() {
    let world = tempfile::tempdir().unwrap();
    let (layer, wire) = layer_over(world.path(), golden_routes(), Duration::from_millis(80));
    let started = std::time::Instant::now();
    // Two misses through the one cache: the version entry, then the package
    // entry. Lower bound only — a sleep never returns early, so this cannot
    // flake (http_cache's own throttle test).
    layer.licenses_for(TIME_KEY).await.unwrap().unwrap();
    layer.deprecation_for(TIME_KEY).await.unwrap().unwrap();
    assert!(started.elapsed() >= Duration::from_millis(80));
    assert_eq!(wire.calls(), 2);
    // The same questions again are cache hits: no request, no throttle slot.
    layer.licenses_for(TIME_KEY).await.unwrap().unwrap();
    layer.deprecation_for(TIME_KEY).await.unwrap().unwrap();
    assert_eq!(wire.calls(), 2);
}

// ---------------------------------------------------------------------------
// The offline guarantee: no deps.dev request anywhere on the sync path.
// ---------------------------------------------------------------------------

#[test]
fn a_full_sync_performs_no_deps_dev_requests() {
    use crate::local::{self, sync_folder};
    use crate::store::Store;
    // Any deps.dev request this thread makes panics, so a sync that asked
    // for one fact would fail here: the offline guarantee of plan §4 P2 is
    // proven by the wire staying silent.
    let wire = on_demand::install_panic();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("store")).unwrap();
    let session = local::session(&store, true).unwrap();
    let root = dir.path().join("app");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.lock"), LOCK_WITH_DEPS).unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        "use serde::Deserialize;\n\nfn main() {}\n",
    )
    .unwrap();

    let synced = sync_folder(&store, &session, &root, None).unwrap();
    // The lockfile became package facts: the sync did its supply-chain work.
    assert_eq!(synced["cargo_lock"]["versions"], json!(5), "{synced:#?}");
    assert_eq!(wire.calls(), 0);

    // Nothing queued would fetch ecosystem-wide either: the only
    // supply-chain job a sync queues is OSV — the one eager exception, when
    // sync.osv leaves it on — and no job kind names deps.dev.
    let repository = s(&synced, "repository_id");
    let kinds: Vec<String> = store
        .graph()
        .read(|db| {
            store_graph::rows(
                db,
                "MATCH (j:Job {repository_id: $repo}) RETURN DISTINCT j.kind AS kind ORDER BY kind",
                store_graph::params(&json!({"repo": repository})),
            )
        })
        .unwrap()
        .into_iter()
        .filter_map(|row| row["kind"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        kinds.iter().any(|kind| kind == "osv"),
        crate::supply_chain::osv::enabled(),
        "OSV is the one eager exception, and it is unchanged: {kinds:?}"
    );
    assert!(
        kinds.iter().all(|kind| !kind.contains("deps")),
        "no deps.dev job kind exists to enqueue: {kinds:?}"
    );
}

#[tokio::test]
#[should_panic(expected = "deps.dev was fetched")]
async fn an_installed_panic_wire_fails_any_fetch() {
    // The stub the offline guarantee hangs on, proven live: a fetch through
    // it panics, so the sync test above stays meaningful.
    on_demand::install_panic();
    let world = tempfile::tempdir().unwrap();
    let layer = on_demand::on(world.path()).unwrap();
    let _ = layer.licenses_for(TIME_KEY).await;
}

#[tokio::test]
async fn the_on_entry_serves_through_the_installed_wire() {
    on_demand::install_fixture(Arc::new(FixtureTransport::new(
        golden_routes()
            .into_iter()
            .map(|(url, body)| (url, json(body))),
    )));
    let world = tempfile::tempdir().unwrap();
    let layer = on_demand::on(world.path()).unwrap();
    assert_eq!(
        layer.licenses_for(TIME_KEY).await.unwrap(),
        Some(licenses(TIME_KEY, &["MIT OR Apache-2.0"]))
    );
}

// ---------------------------------------------------------------------------
// The pin write path: provenance, the analysis reference, the re-run.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_pinned_answer_re_runs_from_pins_against_a_changed_world() {
    let graph = graph_with_lock();
    // The world as the first run read it.
    let world = tempfile::tempdir().unwrap();
    let (t0, wire_t0) = layer_over(world.path(), golden_routes(), Duration::ZERO);
    let first = license_answer(&graph, REPO, &t0, &[TIME_KEY], AS_OF)
        .await
        .unwrap();
    assert_eq!(
        first.rows,
        [json!({"purl_at_version": TIME_KEY,
                "expressions": ["MIT OR Apache-2.0"],
                "origin": "deps.dev", "as_of": AS_OF})],
        "{:#?}",
        first.rows
    );
    assert_eq!(wire_t0.calls(), 1);

    // The answer must outlive the question: its consumed facts are pinned.
    let report = pin(&graph, REPO, &first.consumed, "report-7", KNOWN_AT, AS_OF).unwrap();
    assert_eq!(report["licenses"], json!(1), "{report:#?}");

    // The clock moves and the world changes: a fresh cache, a transport
    // that answers differently — the license is Apache-2.0 alone now.
    let later = tempfile::tempdir().unwrap();
    let (t1, wire_t1) = layer_over(later.path(), changed_routes(), Duration::ZERO);
    let rerun = license_answer(&graph, REPO, &t1, &[TIME_KEY], AS_OF)
        .await
        .unwrap();
    // Identical citations, answered from the pins.
    assert_eq!(rerun.rows, first.rows);
    assert_eq!(wire_t1.calls(), 0);

    // A version the pin does not cover falls through to the changed world:
    // serde's license is fetched, and it is the changed one — proof that
    // time's row above came from the pin, not from a stale cache.
    let serde = license_answer(&graph, REPO, &t1, &[SERDE_KEY], AS_OF)
        .await
        .unwrap();
    assert_eq!(wire_t1.calls(), 1);
    assert_eq!(serde.rows[0]["expressions"], json!(["Apache-2.0"]));

    // Re-pinning the re-run's answer is idempotent: the same edge, the same
    // state (lock.rs's re-ingest discipline).
    let before = store_graph::test_state(&graph);
    let again = pin(&graph, REPO, &rerun.consumed, "report-7", KNOWN_AT, AS_OF).unwrap();
    assert_eq!(store_graph::test_state(&graph), before);
    assert_eq!(again, report);
}

#[tokio::test]
async fn pins_write_every_family_with_provenance_and_the_analysis_reference() {
    let graph = graph_with_lock();
    let world = tempfile::tempdir().unwrap();
    let (layer, _) = layer_over(world.path(), golden_routes(), Duration::ZERO);
    // One of every fact kind, as an analysis that asked every question
    // consumed them.
    let Fact::SourceRepo { url, .. } = layer.source_repo_for(TIME_KEY).await.unwrap().unwrap()
    else {
        unreachable!()
    };
    let facts = vec![
        layer.licenses_for(TIME_KEY).await.unwrap().unwrap(),
        layer.deprecation_for(TIME_KEY).await.unwrap().unwrap(),
        Fact::SourceRepo {
            purl_at_version: TIME_KEY.into(),
            url: url.clone(),
        },
        layer
            .scorecard_for(TIME_KEY, &project_id(&url))
            .await
            .unwrap()
            .unwrap(),
        layer.requirements_for(TIME_KEY).await.unwrap().unwrap(),
    ];
    let report = pin(&graph, REPO, &facts, "report-1", KNOWN_AT, AS_OF).unwrap();
    assert_eq!(
        report,
        json!({
            "origin": "deps.dev", "analysis": "report-1", "known_at": KNOWN_AT,
            "as_of": AS_OF, "licenses": 1, "deprecations": 1, "source_repos": 1,
            "scorecards": 1, "requires_joined": 2,
            "requires_unjoined": ["rand", "winapi"], "unmatched": [],
        }),
        "{report:#?}"
    );

    // Licenses: plan §2's edge and node, with the pin's provenance on the
    // edge and the SPDX expression as the License node's key.
    let edge = row(
        &graph,
        "MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: $key})\
         -[r:LICENSED_AS]->(l:License) \
         RETURN r.expression AS expression, l.spdx AS spdx, r.origin AS origin, \
                r.known_at AS known_at, r.as_of AS as_of, r.analysis AS analysis",
        json!({"repo": REPO, "key": TIME_KEY}),
    );
    assert_eq!(
        edge,
        json!({"expression": "MIT OR Apache-2.0", "spdx": "MIT OR Apache-2.0",
               "origin": "deps.dev", "known_at": KNOWN_AT, "as_of": AS_OF,
               "analysis": "report-1"}),
        "{edge}"
    );

    // The other families are origin-tagged properties on the version.
    let version = row(
        &graph,
        "MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: $key}) \
         RETURN pv.is_deprecated AS deprecated, pv.deprecation_origin AS deprecation_origin, \
                pv.deprecation_as_of AS deprecation_as_of, \
                pv.deprecation_analysis AS deprecation_analysis, \
                pv.source_repo AS source_repo, pv.source_repo_origin AS source_repo_origin, \
                pv.source_repo_as_of AS source_repo_as_of, \
                pv.source_repo_analysis AS source_repo_analysis, \
                pv.scorecard_project AS project, pv.scorecard_stars AS stars, \
                pv.scorecard_overall AS overall, pv.scorecard_origin AS scorecard_origin, \
                pv.scorecard_as_of AS scorecard_as_of, pv.scorecard_analysis AS scorecard_analysis",
        json!({"repo": REPO, "key": TIME_KEY}),
    );
    assert_eq!(version["deprecated"], json!(false));
    assert_eq!(version["deprecation_origin"], json!("deps.dev"));
    assert_eq!(version["deprecation_as_of"], json!(AS_OF));
    assert_eq!(version["deprecation_analysis"], json!("report-1"));
    assert_eq!(
        version["source_repo"],
        json!("https://github.com/time-rs/time")
    );
    assert_eq!(version["source_repo_origin"], json!("deps.dev"));
    assert_eq!(version["source_repo_as_of"], json!(AS_OF));
    assert_eq!(version["source_repo_analysis"], json!("report-1"));
    assert_eq!(version["project"], json!("github.com/time-rs/time"));
    assert_eq!(version["stars"], json!(1347));
    assert_eq!(version["overall"], json!(4.6));
    assert_eq!(version["scorecard_origin"], json!("deps.dev"));
    assert_eq!(version["scorecard_as_of"], json!(AS_OF));
    assert_eq!(version["scorecard_analysis"], json!("report-1"));

    // The pinned reads return the facts exactly as they were consumed.
    assert_eq!(
        pinned_licenses(&graph, REPO, TIME_KEY, AS_OF).unwrap(),
        Some(facts[0].clone())
    );
    assert_eq!(
        pinned_deprecation(&graph, REPO, TIME_KEY, AS_OF).unwrap(),
        Some(facts[1].clone())
    );
    assert_eq!(
        pinned_source_repo(&graph, REPO, TIME_KEY, AS_OF).unwrap(),
        Some(facts[2].clone())
    );
    let scorecard = pinned_scorecard(&graph, REPO, TIME_KEY, AS_OF)
        .unwrap()
        .unwrap();
    assert_eq!(scorecard, facts[3]);
}

#[test]
fn pinned_requirements_join_onto_the_lockfiles_requires_edges() {
    let graph = graph_with_lock();
    // The manifest of time 0.2.22 as deps.dev reports it: two constraints
    // the lockfile resolved versions for, one dev constraint and one
    // disabled optional it did not.
    let manifest = Fact::Requirements {
        purl_at_version: TIME_KEY.into(),
        dependencies: vec![
            requirement("const_fn", "^0.4.2", "normal", false, &[], ""),
            requirement("libc", "^0.2", "normal", true, &[], "cfg(unix)"),
            requirement("rand", "^0.7", "dev", false, &[], ""),
            requirement(
                "winapi",
                "^0.3",
                "normal",
                true,
                &["minwinbase", "minwindef", "timezoneapi"],
                "cfg(windows)",
            ),
        ],
    };
    let report = pin(&graph, REPO, &[manifest], "report-2", KNOWN_AT, AS_OF).unwrap();
    // The unjoinable constraints are reported, never dropped silently.
    assert_eq!(report["requires_joined"], json!(2), "{report:#?}");
    assert_eq!(report["requires_unjoined"], json!(["rand", "winapi"]));

    // The lockfile's resolved edge and the manifest's constraint edge are
    // two edges: one says the resolved graph has the dependency, the other
    // how the manifest declared it — the enrichment the lockfile deferred.
    let resolved = row(
        &graph,
        "MATCH (a:PackageVersion {repository_id: $repo, purl_at_version: $from})\
         -[r:REQUIRES {origin: 'cargo.lock'}]->\
         (b:PackageVersion {repository_id: $repo, purl_at_version: $to}) \
         RETURN r.requirement AS requirement",
        json!({"repo": REPO, "from": TIME_KEY,
               "to": "pkg:cargo/const_fn@0.4.9"}),
    );
    assert_eq!(
        resolved["requirement"],
        Value::Null,
        "the lockfile says none"
    );
    let joined = rows(
        &graph,
        "MATCH (v:PackageVersion {repository_id: $repo, purl_at_version: $key})\
         -[r:REQUIRES {origin: 'deps.dev'}]->(t:PackageVersion {repository_id: $repo}) \
         RETURN t.name AS name, r.requirement AS requirement, r.kind AS kind, \
                r.optional AS optional, r.features AS features, r.target AS target, \
                r.origin AS origin, r.known_at AS known_at, r.as_of = $as_of AS pinned \
         ORDER BY name",
        json!({"repo": REPO, "key": TIME_KEY, "as_of": AS_OF}),
    );
    assert_eq!(
        joined,
        [
            json!({"name": "const_fn", "requirement": "^0.4.2", "kind": "normal",
                   "optional": false, "features": "[]", "target": "",
                   "origin": "deps.dev", "known_at": KNOWN_AT, "pinned": true}),
            json!({"name": "libc", "requirement": "^0.2", "kind": "normal",
                   "optional": true, "features": "[]", "target": "cfg(unix)",
                   "origin": "deps.dev", "known_at": KNOWN_AT, "pinned": true}),
        ],
        "{joined:?}"
    );

    // The pinned read returns the constraints the lockfile could resolve,
    // exactly as they were consumed.
    let pinned = pinned_requirements(&graph, REPO, TIME_KEY, AS_OF)
        .unwrap()
        .unwrap();
    assert_eq!(
        pinned,
        Fact::Requirements {
            purl_at_version: TIME_KEY.into(),
            dependencies: vec![
                requirement("const_fn", "^0.4.2", "normal", false, &[], ""),
                requirement("libc", "^0.2", "normal", true, &[], "cfg(unix)"),
            ],
        }
    );
    assert_eq!(
        pinned_requirements(&graph, REPO, "pkg:cargo/serde@1.0.200", AS_OF).unwrap(),
        None
    );
}

#[test]
fn pinned_reads_match_on_as_of_and_report_what_they_cannot_pin() {
    let graph = graph_with_lock();
    let fact = licenses(TIME_KEY, &["MIT OR Apache-2.0"]);
    pin(
        &graph,
        REPO,
        std::slice::from_ref(&fact),
        "report-3",
        KNOWN_AT,
        AS_OF,
    )
    .unwrap();
    // The pin matches its own as_of ...
    assert_eq!(
        pinned_licenses(&graph, REPO, TIME_KEY, AS_OF).unwrap(),
        Some(fact)
    );
    // ... and no other: another as_of finds no pin, so that answer falls
    // through to the read-through layer.
    assert_eq!(
        pinned_licenses(&graph, REPO, TIME_KEY, "2026-12-01T00:00:00Z").unwrap(),
        None
    );
    // A fact about a version the store does not hold is reported, never
    // pinned: pins are written on the lockfile's own versions.
    let ghost = Fact::Deprecation {
        purl_at_version: "pkg:cargo/ghost@1.0.0".into(),
        deprecated: true,
        reason: None,
    };
    let report = pin(&graph, REPO, &[ghost], "report-3", KNOWN_AT, AS_OF).unwrap();
    assert_eq!(
        report["unmatched"],
        json!(["pkg:cargo/ghost@1.0.0"]),
        "{report:#?}"
    );
    assert_eq!(report["deprecations"], json!(0));
    assert_eq!(
        pinned_deprecation(&graph, REPO, "pkg:cargo/ghost@1.0.0", AS_OF).unwrap(),
        None
    );
}
