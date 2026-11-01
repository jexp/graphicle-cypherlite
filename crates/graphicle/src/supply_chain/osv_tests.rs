//! The OSV client and writer, over a fixture transport and an in-memory
//! store. The golden answers are the `time` 0.2.22 advisory (RUSTSEC-2020-0071
//! and its GHSA alias) and a clean `serde` 1.0.200, under
//! `tests/fixtures/supply_chain/osv/`. No test reaches the network.

use super::lock;
use super::osv::{self, Client, Scan, Target, Vuln, fixed_at_or_above, scan, targets, write};
use crate::store_graph::{self, GraphStore};
use crate::supply_chain::http_cache::HttpCache;
use crate::supply_chain::http_cache::test_transport::{FixtureTransport, json};
use cargo_lock::Version;
use serde_json::{Value, json as value};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

const QUERYBATCH: &str =
    include_str!("../../tests/fixtures/supply_chain/osv/querybatch_time_serde.json");
const RUSTSEC: &str =
    include_str!("../../tests/fixtures/supply_chain/osv/vuln_rustsec_2020_0071.json");
const GHSA: &str =
    include_str!("../../tests/fixtures/supply_chain/osv/vuln_ghsa_wcg3_cvx6_7396.json");

const REPO: &str = "repo-a";
const KNOWN_AT: &str = "2026-11-01T04:00:00Z";
const TIME_KEY: &str = "pkg:cargo/time@0.2.22";
const SERDE_KEY: &str = "pkg:cargo/serde@1.0.200";

/// The project's lockfile: a path package (`app`, not queried), and two
/// crates.io packages.
pub(crate) const LOCK: &str = r#"version = 4

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
checksum = "1111111111111111111111111111111111111111111111111111111111111111"
"#;

fn base() -> String {
    osv::BASE_URL.to_owned()
}

/// A client whose wire is the fixture: the querybatch answer and both
/// advisory details. The returned transport counts what reached it.
fn client(store: &TempDir, batch: &str) -> (Client<Arc<FixtureTransport>>, Arc<FixtureTransport>) {
    let transport = Arc::new(FixtureTransport::new([
        (format!("{}/querybatch", base()), json(batch)),
        (format!("{}/vulns/RUSTSEC-2020-0071", base()), json(RUSTSEC)),
        (format!("{}/vulns/GHSA-wcg3-cvx6-7396", base()), json(GHSA)),
    ]));
    let cache = HttpCache::for_store(store.path(), osv::NAMESPACE, Duration::ZERO);
    (Client::new(transport.clone(), cache), transport)
}

/// An in-memory store holding the lockfile's package facts for `REPO`.
fn graph_with_lock() -> GraphStore {
    let graph = GraphStore::open_in_memory().unwrap();
    lock::write(&graph, REPO, &lock::parse(LOCK).unwrap()).unwrap();
    graph
}

/// The rows of a read-only query over the graph.
fn rows(graph: &GraphStore, cypher: &str) -> Vec<Value> {
    graph
        .read(|db| store_graph::rows(db, cypher, store_graph::params(&value!({"repo": REPO}))))
        .unwrap()
}

fn target(key: &str, name: &str, version: &str) -> Target {
    Target {
        key: key.to_owned(),
        name: name.to_owned(),
        version: version.to_owned(),
    }
}

/// The two registry targets of the lockfile, as the graph names them.
fn lock_targets(graph: &GraphStore) -> Vec<Target> {
    targets(graph, REPO).unwrap()
}

fn vuln(text: &str) -> Vuln {
    serde_json::from_str(text).unwrap()
}

#[test]
fn only_crates_io_registry_packages_are_targets() {
    let graph = graph_with_lock();
    // `app` is a path package with no source line: no OSV query names it.
    assert_eq!(
        lock_targets(&graph),
        [
            target(SERDE_KEY, "serde", "1.0.200"),
            target(TIME_KEY, "time", "0.2.22"),
        ]
    );
}

#[tokio::test]
async fn one_querybatch_post_names_the_whole_lockfile() {
    let graph = graph_with_lock();
    let store = TempDir::new().unwrap();
    let (client, transport) = client(&store, QUERYBATCH);
    scan(&client, &lock_targets(&graph)).await.unwrap();

    // One POST for both packages, with the queries in target order.
    let posted = transport.posted();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(
        serde_json::from_str::<Value>(&posted[0]).unwrap(),
        value!({"queries": [
            {"package": {"name": "serde", "ecosystem": "crates.io"}, "version": "1.0.200"},
            {"package": {"name": "time", "ecosystem": "crates.io"}, "version": "0.2.22"},
        ]})
    );
    // The two advisory details follow, one GET each: three requests in all.
    assert_eq!(transport.calls(), 3);

    // The same lockfile again is served from the cache: no new POST.
    scan(&client, &lock_targets(&graph)).await.unwrap();
    assert_eq!(transport.posted().len(), 1);
    assert_eq!(transport.calls(), 3);
}

#[tokio::test]
async fn the_time_advisory_flags_0_2_22_and_names_0_2_23() {
    let graph = graph_with_lock();
    let store = TempDir::new().unwrap();
    let (client, _transport) = client(&store, QUERYBATCH);
    let scan = scan(&client, &lock_targets(&graph)).await.unwrap();

    let time_hits: Vec<(&str, Option<&str>)> = scan
        .hits
        .iter()
        .filter(|hit| hit.key == TIME_KEY)
        .map(|hit| (hit.osv_id.as_str(), hit.fixed_in.as_deref()))
        .collect();
    assert_eq!(
        time_hits,
        [
            ("GHSA-wcg3-cvx6-7396", Some("0.2.23")),
            ("RUSTSEC-2020-0071", Some("0.2.23")),
        ]
    );
    // serde has no hit: it is clean, not affected.
    assert!(scan.hits.iter().all(|hit| hit.key != SERDE_KEY));
    assert_eq!(scan.clean, [SERDE_KEY]);
    assert!(scan.truncated.is_empty());
}

#[tokio::test]
async fn the_advisory_facts_come_from_the_detail() {
    let graph = graph_with_lock();
    let store = TempDir::new().unwrap();
    let (client, _transport) = client(&store, QUERYBATCH);
    let scan = scan(&client, &lock_targets(&graph)).await.unwrap();

    let rustsec = &scan.advisories["RUSTSEC-2020-0071"];
    assert_eq!(
        rustsec.title.as_deref(),
        Some("Potential segfault in the time crate")
    );
    assert_eq!(
        rustsec.url.as_deref(),
        Some("https://rustsec.org/advisories/RUSTSEC-2020-0071.html")
    );
    assert_eq!(
        rustsec.cvss3_vector.as_deref(),
        Some("CVSS:3.1/AV:L/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:H")
    );
    assert!(rustsec.aliases.contains(&"GHSA-wcg3-cvx6-7396".to_owned()));
}

#[tokio::test]
async fn the_writer_stores_affected_by_with_provenance_and_the_clean_scan() {
    let graph = graph_with_lock();
    let store = TempDir::new().unwrap();
    let (client, _transport) = client(&store, QUERYBATCH);
    let scan = scan(&client, &lock_targets(&graph)).await.unwrap();
    let result = write(&graph, REPO, &scan, KNOWN_AT).unwrap();

    // The result names the fixed version for the locked one.
    assert_eq!(
        result["affected"],
        value!([
            {"purl_at_version": TIME_KEY, "osv_id": "GHSA-wcg3-cvx6-7396", "fixed_in": "0.2.23"},
            {"purl_at_version": TIME_KEY, "osv_id": "RUSTSEC-2020-0071", "fixed_in": "0.2.23"},
        ])
    );
    assert_eq!(result["clean"], value!(1));

    // AFFECTED_BY on the locked version, to each advisory, with its provenance
    // and the fixed version it names.
    let affected = rows(
        &graph,
        "MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: 'pkg:cargo/time@0.2.22'})\
         -[r:AFFECTED_BY]->(a:Advisory {repository_id: $repo}) \
         RETURN a.osv_id AS osv_id, r.osv_id AS edge_osv_id, r.source AS source, \
         r.cvss3_vector AS cvss3_vector, r.fixed_in AS fixed_in, r.known_at AS known_at",
    );
    assert_eq!(affected.len(), 2, "{affected:?}");
    let rustsec = affected
        .iter()
        .find(|row| row["osv_id"] == "RUSTSEC-2020-0071")
        .expect("the RUSTSEC edge");
    assert_eq!(rustsec["edge_osv_id"], "RUSTSEC-2020-0071");
    assert_eq!(rustsec["source"], "osv");
    assert_eq!(rustsec["known_at"], KNOWN_AT);
    assert_eq!(rustsec["fixed_in"], "0.2.23");
    assert_eq!(
        rustsec["cvss3_vector"],
        "CVSS:3.1/AV:L/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:H"
    );

    // The Advisory node carries its title, url and aliases.
    let advisory = rows(
        &graph,
        "MATCH (a:Advisory {repository_id: $repo, osv_id: 'RUSTSEC-2020-0071'}) \
         RETURN a.title AS title, a.url AS url, a.aliases AS aliases",
    );
    assert_eq!(advisory[0]["title"], "Potential segfault in the time crate");
    assert_eq!(
        advisory[0]["url"],
        "https://rustsec.org/advisories/RUSTSEC-2020-0071.html"
    );
    assert!(
        advisory[0]["aliases"]
            .as_str()
            .unwrap()
            .contains("GHSA-wcg3-cvx6-7396")
    );

    // serde is SCANNED_CLEAN as of the query, and time is not.
    let clean = rows(
        &graph,
        "MATCH (pv:PackageVersion {repository_id: $repo})-[r:SCANNED_CLEAN]->\
         (c:AdvisoryCorpus {repository_id: $repo}) \
         RETURN pv.purl_at_version AS key, r.as_of AS as_of, r.source AS source",
    );
    assert_eq!(
        clean,
        [value!({"key": SERDE_KEY, "as_of": KNOWN_AT, "source": "osv"})]
    );
}

#[tokio::test]
async fn a_rescan_replaces_the_edges_it_no_longer_reports() {
    let graph = graph_with_lock();
    let store = TempDir::new().unwrap();
    let (client, _transport) = client(&store, QUERYBATCH);
    let first = scan(&client, &lock_targets(&graph)).await.unwrap();
    write(&graph, REPO, &first, KNOWN_AT).unwrap();

    // A later scan that finds nothing removes both edges; the advisories stay.
    let empty = Scan {
        queried: 2,
        clean: vec![SERDE_KEY.to_owned(), TIME_KEY.to_owned()],
        ..Scan::default()
    };
    write(&graph, REPO, &empty, "2026-11-02T04:00:00Z").unwrap();
    assert!(
        rows(
            &graph,
            "MATCH (:PackageVersion {repository_id: $repo})-[r:AFFECTED_BY]->() RETURN r.osv_id AS osv_id",
        )
        .is_empty()
    );
    assert_eq!(
        rows(
            &graph,
            "MATCH (a:Advisory {repository_id: $repo}) RETURN a.osv_id AS osv_id"
        )
        .len(),
        2
    );
    assert_eq!(
        rows(
            &graph,
            "MATCH (:PackageVersion {repository_id: $repo})-[r:SCANNED_CLEAN]->() RETURN r.as_of AS as_of",
        )
        .len(),
        2
    );
}

#[tokio::test]
async fn a_paged_answer_claims_no_clean_scan() {
    let graph = graph_with_lock();
    let store = TempDir::new().unwrap();
    // Serde's answer is only the first page: it is neither clean nor affected.
    let paged = r#"{"results": [{"vulns": [], "next_page_token": "page-2"}, {}]}"#;
    let (client, _transport) = client(&store, paged);
    let scan = scan(&client, &lock_targets(&graph)).await.unwrap();
    assert_eq!(scan.truncated, [SERDE_KEY]);
    assert_eq!(scan.clean, [TIME_KEY]);
    let result = write(&graph, REPO, &scan, KNOWN_AT).unwrap();
    assert_eq!(result["truncated"], value!([SERDE_KEY]));
}

#[tokio::test]
async fn a_truncated_rescan_keeps_the_evidence_of_the_versions_it_did_not_finish() {
    let graph = graph_with_lock();
    let store = TempDir::new().unwrap();
    // A complete scan first: serde clean, time affected by both advisories.
    let (complete, _transport) = client(&store, QUERYBATCH);
    let first = scan(&complete, &lock_targets(&graph)).await.unwrap();
    write(&graph, REPO, &first, KNOWN_AT).unwrap();

    // A later, truncated scan: serde's answer is only the first page, so
    // serde claims nothing — and the prior scan's evidence survives it.
    let later = TempDir::new().unwrap();
    let paged = r#"{"results": [{"vulns": [], "next_page_token": "page-2"}, {}]}"#;
    let (paged_client, _transport) = client(&later, paged);
    let second = scan(&paged_client, &lock_targets(&graph)).await.unwrap();
    let result = write(&graph, REPO, &second, "2026-11-02T04:00:00Z").unwrap();
    assert_eq!(result["truncated"], value!([SERDE_KEY]));

    // serde's clean edge is the complete scan's, at its own as_of; time, a
    // finished key, is rewritten — its advisories give way to the clean scan
    // of the later answer.
    assert_eq!(
        rows(
            &graph,
            "MATCH (pv:PackageVersion {repository_id: $repo})-[r:SCANNED_CLEAN]->\
             (:AdvisoryCorpus {repository_id: $repo}) \
             RETURN pv.purl_at_version AS key, r.as_of AS as_of ORDER BY key"
        ),
        [
            value!({"key": SERDE_KEY, "as_of": KNOWN_AT}),
            value!({"key": TIME_KEY, "as_of": "2026-11-02T04:00:00Z"}),
        ]
    );
    assert!(
        rows(
            &graph,
            "MATCH (:PackageVersion {repository_id: $repo})-[r:AFFECTED_BY]->() \
         RETURN r.osv_id AS osv_id"
        )
        .is_empty()
    );

    // A scan that finished nothing (every answer paged) erases no evidence
    // either: both versions keep the edges the last finished scan wrote.
    let none = Scan {
        queried: 2,
        truncated: vec![SERDE_KEY.to_owned(), TIME_KEY.to_owned()],
        ..Scan::default()
    };
    write(&graph, REPO, &none, "2026-11-03T04:00:00Z").unwrap();
    assert_eq!(
        rows(
            &graph,
            "MATCH (pv:PackageVersion {repository_id: $repo})-[r:SCANNED_CLEAN]->\
             (:AdvisoryCorpus {repository_id: $repo}) \
             RETURN pv.purl_at_version AS key, r.as_of AS as_of ORDER BY key"
        ),
        [
            value!({"key": SERDE_KEY, "as_of": KNOWN_AT}),
            value!({"key": TIME_KEY, "as_of": "2026-11-02T04:00:00Z"}),
        ]
    );
}

#[tokio::test]
async fn a_malformed_answer_fails_the_scan_and_writes_nothing() {
    let graph = graph_with_lock();
    let store = TempDir::new().unwrap();
    // Two queries, one result: the answer does not match the request.
    let (client, _transport) = client(&store, r#"{"results": [{}]}"#);
    let error = scan(&client, &lock_targets(&graph)).await.unwrap_err();
    assert!(error.message.contains("1 results for 2 queries"), "{error}");
}

#[test]
fn the_fixed_version_is_the_smallest_fix_at_or_above_the_locked_one() {
    let time = vuln(RUSTSEC);
    let locked = Version::parse("0.2.22").unwrap();
    assert_eq!(
        fixed_at_or_above(&time, "time", &locked).map(|v| v.to_string()),
        Some("0.2.23".to_owned())
    );
    // Above every fix, nothing is named.
    assert_eq!(
        fixed_at_or_above(&time, "time", &Version::parse("0.2.24").unwrap()),
        None
    );
    // An exact fix counts, and the fixes below the locked version do not.
    assert_eq!(
        fixed_at_or_above(&time, "time", &Version::parse("0.2.6").unwrap()).map(|v| v.to_string()),
        Some("0.2.6".to_owned())
    );
    // Another crate's ranges do not count.
    assert_eq!(fixed_at_or_above(&time, "chrono", &locked), None);
}

#[test]
fn ranges_without_a_semver_fix_name_nothing() {
    let advisory = vuln(
        r#"{"id": "X-1", "affected": [{"package": {"name": "demo", "ecosystem": "crates.io"},
            "ranges": [{"type": "GIT", "events": [{"introduced": "0"}, {"fixed": "3f2a9c1"}]},
                       {"type": "SEMVER", "events": [{"introduced": "1.0.0"}, {"fixed": "1.4.0"}]}]}]}"#,
    );
    let locked = Version::parse("1.2.0").unwrap();
    assert_eq!(
        fixed_at_or_above(&advisory, "demo", &locked).map(|v| v.to_string()),
        Some("1.4.0".to_owned())
    );
}

#[test]
fn the_input_hash_is_the_digest_and_the_format() {
    assert_eq!(osv::input_hash("abc"), osv::input_hash("abc"));
    assert_ne!(osv::input_hash("abc"), osv::input_hash("abd"));
    assert_eq!(osv::FORMAT, "querybatch.v1");
}

#[test]
fn the_switch_is_off_by_environment_then_by_configuration() {
    // The environment variable wins over the configuration, both ways.
    assert!(!osv::decide(Some("0"), Some(true)));
    assert!(!osv::decide(Some(" OFF "), None));
    assert!(osv::decide(Some("1"), Some(false)));
    // Without the variable, the configured setting; without that, on.
    assert!(!osv::decide(None, Some(false)));
    assert!(osv::decide(None, Some(true)));
    assert!(osv::decide(None, None));
}
