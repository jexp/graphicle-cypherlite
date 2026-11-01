//! The OSV advisory query of a project's lockfile (supply-chain plan §1, §2,
//! §4 P2). One bulk `querybatch` POST names, per registry package version,
//! the advisories that affect it. One `GET /v1/vulns/{id}` per distinct
//! advisory supplies its title, aliases, advisory URL, CVSS vector and affected
//! ranges. The facts are written as `(:Advisory)` nodes, `AFFECTED_BY` edges
//! from the versions they touch, and `SCANNED_CLEAN` edges from the versions
//! the query found no advisory for.
//!
//! # Matching and fixed versions
//!
//! OSV matches versions on its side: every advisory a `querybatch` answer
//! names contains the queried version, so each one is an `AFFECTED_BY` edge.
//! The fixed version is read from the detail's ranges: the smallest `fixed`
//! event at or above the locked version, across the advisory's ranges for
//! that crate. For `time` 0.2.22 both RUSTSEC-2020-0071 and its GHSA alias
//! name 0.2.23. An advisory whose ranges name no such version has no
//! `fixed_in`.
//!
//! # Negative evidence
//!
//! A version the answer lists no advisory for is `SCANNED_CLEAN` as of the
//! query. The edge points at the project's `AdvisoryCorpus` node (source
//! `osv`), so the claim names the corpus it was checked against. A version whose answer was paged (`next_page_token`) is not: the
//! answer is incomplete, so the version is listed under `truncated` and
//! claims nothing.
//!
//! # Scope and identity
//!
//! Every node carries `repository_id`, like every label of the store, and
//! every MERGE names it. The same advisory in two projects is therefore two
//! `Advisory` nodes, keyed by (`repository_id`, `osv_id`). The per-repository
//! advisory database of supply-chain plan §4.1 is where they consolidate.
//! Aliases are kept as the advisory reports them; a GHSA and its RUSTSEC
//! sibling stay two nodes, one per id OSV returned.
//!
//! # Which packages
//!
//! Only registry packages from the crates.io index are queried: OSV's
//! `crates.io` ecosystem is that index. A path, git or alternative-registry
//! package has no such version to match, so it is neither affected nor clean.
//!
//! # Network and offline
//!
//! The network is the job's, not the sync's. The sync only queues the job,
//! keyed on the lockfile's digest ([`input_hash`]). A store that must stay
//! offline sets `sync.osv` to `false` (or `GRAPHICLE_SYNC_OSV=0`), and then
//! nothing is queued and nothing is sent. The lockfile is still read.
use crate::store_graph::{self, GraphStore};
use crate::supply_chain::http_cache::{FetchError, HttpCache, HttpResponse, Transport};
use crate::supply_chain::lock::FILE;
use crate::{Error, Result};
use cargo_lock::Version;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::Duration;

#[cfg(not(test))]
use crate::supply_chain::http_cache::ReqwestTransport;
#[cfg(test)]
use crate::supply_chain::http_cache::test_transport::FixtureTransport;
#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::sync::Arc;

/// The OSV v1 API root.
pub const BASE_URL: &str = "https://api.osv.dev/v1";

/// The [`HttpCache`] namespace for this client's entries.
pub const NAMESPACE: &str = "osv";

/// The gap between OSV requests. OSV publishes no limit, so this is the same
/// conservative posture as the deps.dev client's.
pub const MIN_INTERVAL: Duration = Duration::from_millis(250);

/// The job kind of the advisory query: one per lockfile digest.
pub const KIND: &str = "osv";

/// The version of what the job stores. It is part of the input hash, so a
/// change to what is written queues a new job for an unchanged lockfile.
pub const FORMAT: &str = "querybatch.v1";

/// The environment variable that turns the advisory query off (`0`, `false`,
/// `off` or `no`), which wins over `sync.osv`.
pub const VARIABLE: &str = "GRAPHICLE_SYNC_OSV";

/// The ecosystem name OSV uses for the crates.io index.
const ECOSYSTEM: &str = "crates.io";

/// The source-line markers of the crates.io index in `Cargo.lock`.
const INDEX_MARKERS: [&str; 2] = ["crates.io-index", "index.crates.io"];

/// Queries per `querybatch` request: OSV's limit. A lockfile longer than this
/// takes one POST per chunk.
const QUERIES_PER_REQUEST: usize = 1000;

/// The `source` the corpus and the advisories carry.
const SOURCE: &str = "osv";

static INSTALLED: OnceLock<bool> = OnceLock::new();

/// Make the configuration's `sync.osv` this process's setting.
pub fn install(config: &crate::model::Config) {
    if let Some(on) = config.sync.as_ref().and_then(|sync| sync.osv) {
        let _ = INSTALLED.set(on);
    }
}

/// Whether a sync queues the advisory query: [`VARIABLE`] when it is set, else
/// the installed `sync.osv`, else yes.
pub fn enabled() -> bool {
    decide(
        std::env::var(VARIABLE).ok().as_deref(),
        INSTALLED.get().copied(),
    )
}

/// [`enabled`]'s rule over its two inputs: the environment variable when set,
/// else the configuration's setting, else on.
pub(crate) fn decide(variable: Option<&str>, installed: Option<bool>) -> bool {
    match variable {
        Some(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        None => installed.unwrap_or(true),
    }
}

/// The `input_hash` of the one `osv` job per lockfile digest.
pub fn input_hash(digest: &str) -> String {
    crate::digest(&json!({"kind": KIND, "digest": digest, "format": FORMAT}))
}

/// The OSV client, over an injected transport and its read-through cache.
pub struct Client<T: Transport> {
    transport: T,
    cache: HttpCache,
    base: String,
}

impl<T: Transport> Client<T> {
    /// A client against [`BASE_URL`].
    pub fn new(transport: T, cache: HttpCache) -> Self {
        Self::with_base_url(transport, cache, BASE_URL)
    }

    /// A client against another API root.
    pub fn with_base_url(transport: T, cache: HttpCache, base: &str) -> Self {
        Self {
            transport,
            cache,
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    /// One `querybatch` POST: an answer per query, in query order. The POST
    /// is cached by its body, so an unchanged batch is served from the cache.
    async fn query_batch(&self, queries: &[Query<'_>]) -> Result<Vec<BatchResult>> {
        let url = format!("{}/querybatch", self.base);
        let body = serde_json::to_string(&json!({"queries": queries}))?;
        let bytes = self
            .cache
            .post(&self.transport, &url, &body)
            .await
            .map_err(fetch_error)?;
        let answer: BatchAnswer = serde_json::from_slice(&bytes).map_err(|source| {
            Error::bad(format!("unexpected OSV response from {url}: {source}"))
        })?;
        if answer.results.len() != queries.len() {
            return Err(Error::bad(format!(
                "OSV answered {} results for {} queries",
                answer.results.len(),
                queries.len()
            )));
        }
        Ok(answer.results)
    }

    /// One advisory in full: `GET /vulns/{id}`.
    async fn vuln(&self, id: &str) -> Result<Vuln> {
        // The id becomes a path segment unescaped, so only the shape OSV ids
        // take is accepted (`RUSTSEC-2020-0071`, `GHSA-wcg3-cvx6-7396`).
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(Error::bad(format!(
                "OSV returned a malformed advisory id {id:?}"
            )));
        }
        let url = format!("{}/vulns/{id}", self.base);
        let bytes = self
            .cache
            .get(&self.transport, &url)
            .await
            .map_err(fetch_error)?;
        serde_json::from_slice(&bytes)
            .map_err(|source| Error::bad(format!("unexpected OSV response from {url}: {source}")))
    }
}

fn fetch_error(error: FetchError) -> Error {
    Error::bad(format!("OSV request failed: {error}"))
}

#[derive(Serialize)]
struct Query<'a> {
    package: QueryPackage<'a>,
    version: &'a str,
}

#[derive(Serialize)]
struct QueryPackage<'a> {
    name: &'a str,
    ecosystem: &'static str,
}

#[derive(Deserialize)]
struct BatchAnswer {
    results: Vec<BatchResult>,
}

/// One query's answer: the advisories it names, and a token when OSV has more
/// than one page of them.
#[derive(Debug, Clone, Deserialize)]
pub struct BatchResult {
    #[serde(default)]
    pub vulns: Vec<VulnRef>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

/// An advisory id as the batch answer names it.
#[derive(Debug, Clone, Deserialize)]
pub struct VulnRef {
    pub id: String,
}

/// One advisory in full (`GET /v1/vulns/{id}`), with the fields this module
/// reads. The rest of OSV's record is ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct Vuln {
    pub id: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub severity: Vec<Severity>,
    #[serde(default)]
    pub references: Vec<Reference>,
    #[serde(default)]
    pub affected: Vec<Affected>,
}

/// A severity entry: `CVSS_V3` carries the vector as its score.
#[derive(Debug, Clone, Deserialize)]
pub struct Severity {
    #[serde(rename = "type")]
    pub kind: String,
    pub score: String,
}

/// A link on the record; `ADVISORY` names the advisory's own page.
#[derive(Debug, Clone, Deserialize)]
pub struct Reference {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
}

/// The package an advisory affects and the ranges it covers.
#[derive(Debug, Clone, Deserialize)]
pub struct Affected {
    #[serde(default)]
    pub package: Option<AffectedPackage>,
    #[serde(default)]
    pub ranges: Vec<Range>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AffectedPackage {
    pub name: String,
}

/// A range of versions as OSV's event list: `introduced`, `fixed`, ...
#[derive(Debug, Clone, Deserialize)]
pub struct Range {
    #[serde(default)]
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    #[serde(default)]
    pub fixed: Option<String>,
}

/// The smallest fixed version of `vuln` for the crate `name` that is at or
/// above `locked`. `None` when no range names one. A fixed version that does
/// not parse as semver (a git commit, say) is skipped.
pub fn fixed_at_or_above(vuln: &Vuln, name: &str, locked: &Version) -> Option<Version> {
    vuln.affected
        .iter()
        .filter(|affected| {
            affected
                .package
                .as_ref()
                .is_none_or(|package| package.name == name)
        })
        .flat_map(|affected| &affected.ranges)
        .flat_map(|range| &range.events)
        .filter_map(|event| event.fixed.as_deref()?.parse::<Version>().ok())
        .filter(|fixed| fixed >= locked)
        .min()
}

/// One registry package version of the lockfile, as the graph holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Its `purl_at_version`, the key the graph matches it by.
    pub key: String,
    pub name: String,
    pub version: String,
}

/// One advisory a version is affected by, and the fixed version it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub key: String,
    pub osv_id: String,
    pub fixed_in: Option<String>,
}

/// An advisory's facts as the detail reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advisory {
    pub osv_id: String,
    pub title: Option<String>,
    pub aliases: Vec<String>,
    pub url: Option<String>,
    /// The CVSS 3 vector string, when OSV carries one.
    pub cvss3: Option<String>,
}

/// What one query read: the hits, the advisories they name, and the versions
/// the answer found clean or could not finish.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub queried: usize,
    pub hits: Vec<Hit>,
    pub advisories: BTreeMap<String, Advisory>,
    pub clean: Vec<String>,
    pub truncated: Vec<String>,
}

/// Query `targets` in [`QUERIES_PER_REQUEST`] chunks, then read each distinct
/// advisory once. Any failed request fails the scan, so nothing partial is
/// written.
pub async fn scan<T: Transport>(client: &Client<T>, targets: &[Target]) -> Result<Scan> {
    let mut answers = Vec::with_capacity(targets.len());
    for chunk in targets.chunks(QUERIES_PER_REQUEST) {
        let queries: Vec<Query> = chunk
            .iter()
            .map(|target| Query {
                package: QueryPackage {
                    name: &target.name,
                    ecosystem: ECOSYSTEM,
                },
                version: &target.version,
            })
            .collect();
        answers.extend(chunk.iter().zip(client.query_batch(&queries).await?));
    }

    let mut scan = Scan {
        queried: targets.len(),
        ..Scan::default()
    };
    let mut ids = BTreeSet::new();
    for (target, answer) in &answers {
        if answer.next_page_token.is_some() {
            scan.truncated.push(target.key.clone());
        } else if answer.vulns.is_empty() {
            scan.clean.push(target.key.clone());
        }
        ids.extend(answer.vulns.iter().map(|vuln| vuln.id.clone()));
    }

    let mut details = BTreeMap::new();
    for id in ids {
        let vuln = client.vuln(&id).await?;
        details.insert(id, vuln);
    }
    for (target, answer) in &answers {
        if answer.vulns.is_empty() {
            continue;
        }
        let locked = Version::parse(&target.version).map_err(|error| {
            Error::bad(format!("{} is not a semver version: {error}", target.key))
        })?;
        for vuln_ref in &answer.vulns {
            let fixed_in = details
                .get(&vuln_ref.id)
                .and_then(|vuln| fixed_at_or_above(vuln, &target.name, &locked))
                .map(|fixed| fixed.to_string());
            scan.hits.push(Hit {
                key: target.key.clone(),
                osv_id: vuln_ref.id.clone(),
                fixed_in,
            });
        }
    }
    scan.advisories = details
        .values()
        .map(|vuln| (vuln.id.clone(), advisory(vuln)))
        .collect();
    Ok(scan)
}

/// The facts of one advisory detail.
fn advisory(vuln: &Vuln) -> Advisory {
    Advisory {
        osv_id: vuln.id.clone(),
        title: vuln.summary.clone(),
        aliases: vuln.aliases.clone(),
        url: vuln
            .references
            .iter()
            .find(|reference| reference.kind == "ADVISORY")
            .map(|reference| reference.url.clone()),
        cvss3: vuln
            .severity
            .iter()
            .find(|severity| severity.kind == "CVSS_V3")
            .map(|severity| severity.score.clone()),
    }
}

/// The registry packages of `repository_id`'s lockfile, from the graph: the
/// versions its `File` DESCRIBES whose source is the crates.io index.
pub fn targets(graph: &GraphStore, repository_id: &str) -> Result<Vec<Target>> {
    let rows = graph.read(|db| {
        store_graph::rows(
            db,
            "MATCH (f:File {repository_id: $repo, path: $path})-[:DESCRIBES]->\
             (pv:PackageVersion {repository_id: $repo}) \
             RETURN pv.purl_at_version AS key, pv.name AS name, pv.version AS version, \
             pv.source AS source",
            store_graph::params(&json!({"repo": repository_id, "path": FILE})),
        )
    })?;
    let mut targets: Vec<Target> = rows
        .iter()
        .filter(|row| {
            row["source"]
                .as_str()
                .is_some_and(|source| INDEX_MARKERS.iter().any(|marker| source.contains(marker)))
        })
        .filter_map(|row| {
            Some(Target {
                key: row["key"].as_str()?.to_string(),
                name: row["name"].as_str()?.to_string(),
                version: row["version"].as_str()?.to_string(),
            })
        })
        .collect();
    targets.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(targets)
}

/// Write one scan into `repository_id`, stamped `known_at`. The AFFECTED_BY
/// and SCANNED_CLEAN edges of the project are replaced first, so a version the
/// new scan no longer reports loses its edge. Advisory nodes are MERGEd and
/// kept. Returns the job's result.
pub fn write(
    graph: &GraphStore,
    repository_id: &str,
    scan: &Scan,
    known_at: &str,
) -> Result<Value> {
    let params = || store_graph::params(&json!({"repo": repository_id}));
    graph.transaction(|db| {
        store_graph::write(
            db,
            "MATCH (:PackageVersion {repository_id: $repo})-[r:AFFECTED_BY]->() DELETE r",
            params(),
        )?;
        store_graph::write(
            db,
            "MATCH (:PackageVersion {repository_id: $repo})-[r:SCANNED_CLEAN]->() DELETE r",
            params(),
        )?;
        store_graph::write(
            db,
            "MERGE (c:AdvisoryCorpus {repository_id: $repo, source: 'osv'})",
            params(),
        )?;
        Ok(())
    })?;

    let advisories: Vec<Value> = scan
        .advisories
        .values()
        .map(|advisory| {
            json!({"osv_id": advisory.osv_id, "title": advisory.title,
                   "aliases": serde_json::to_string(&advisory.aliases).unwrap_or_default(),
                   "url": advisory.url})
        })
        .collect();
    let hits: Vec<Value> = scan
        .hits
        .iter()
        .map(|hit| {
            json!({"key": hit.key, "osv_id": hit.osv_id, "known_at": known_at,
                   "cvss3": scan.advisories.get(&hit.osv_id).and_then(|a| a.cvss3.clone())})
        })
        .collect();
    let clean: Vec<Value> = scan
        .clean
        .iter()
        .map(|key| json!({"key": key, "as_of": known_at}))
        .collect();
    crate::supply_chain::batches(
        graph,
        repository_id,
        &advisories,
        "UNWIND $rows AS row \
         MERGE (a:Advisory {repository_id: $repo, osv_id: row.osv_id}) \
         SET a.title = row.title, a.aliases = row.aliases, a.url = row.url",
    )?;
    crate::supply_chain::batches(
        graph,
        repository_id,
        &hits,
        "UNWIND $rows AS row \
         MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: row.key}) \
         MATCH (a:Advisory {repository_id: $repo, osv_id: row.osv_id}) \
         MERGE (pv)-[r:AFFECTED_BY {osv_id: row.osv_id}]->(a) \
         SET r.source = 'osv', r.cvss3 = row.cvss3, r.known_at = row.known_at",
    )?;
    crate::supply_chain::batches(
        graph,
        repository_id,
        &clean,
        "UNWIND $rows AS row \
         MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: row.key}) \
         MATCH (c:AdvisoryCorpus {repository_id: $repo, source: 'osv'}) \
         MERGE (pv)-[r:SCANNED_CLEAN]->(c) \
         SET r.source = 'osv', r.as_of = row.as_of",
    )?;

    let affected: Vec<Value> = scan
        .hits
        .iter()
        .map(|hit| {
            json!({"purl_at_version": hit.key, "osv_id": hit.osv_id, "fixed_in": hit.fixed_in})
        })
        .collect();
    Ok(json!({
        "source": SOURCE,
        "queried": scan.queried,
        "advisories": scan.advisories.len(),
        "affected": affected,
        "clean": scan.clean.len(),
        "truncated": scan.truncated,
        "known_at": known_at,
    }))
}

/// The transport a query runs over: the network in production, a fixture in
/// tests. The tests never reach the network: a test without a fixture gets
/// [`Wire::Refused`].
pub(crate) enum Wire {
    #[cfg(not(test))]
    Network(ReqwestTransport),
    #[cfg(test)]
    Fixture(Arc<FixtureTransport>),
    #[cfg(test)]
    Refused,
}

impl Transport for Wire {
    async fn get(&self, url: &str) -> std::result::Result<HttpResponse, FetchError> {
        match self {
            #[cfg(not(test))]
            Self::Network(transport) => transport.get(url).await,
            #[cfg(test)]
            Self::Fixture(transport) => transport.get(url).await,
            #[cfg(test)]
            Self::Refused => Err(refused()),
        }
    }

    async fn post(&self, url: &str, body: &str) -> std::result::Result<HttpResponse, FetchError> {
        match self {
            #[cfg(not(test))]
            Self::Network(transport) => transport.post(url, body).await,
            #[cfg(test)]
            Self::Fixture(transport) => transport.post(url, body).await,
            #[cfg(test)]
            Self::Refused => Err(refused()),
        }
    }
}

#[cfg(test)]
fn refused() -> FetchError {
    FetchError::Transport("the tests do not reach the network".to_owned())
}

#[cfg(test)]
thread_local! {
    static FIXTURE: RefCell<Option<Arc<FixtureTransport>>> = const { RefCell::new(None) };
}

/// Serve this thread's OSV queries from `fixture`.
#[cfg(test)]
pub(crate) fn install_fixture(fixture: Arc<FixtureTransport>) {
    FIXTURE.with(|slot| *slot.borrow_mut() = Some(fixture));
}

/// The wire of the next query, built on the calling thread.
#[cfg(test)]
pub(crate) fn wire() -> Result<Wire> {
    Ok(FIXTURE
        .with(|slot| slot.borrow().clone())
        .map_or(Wire::Refused, Wire::Fixture))
}

/// The wire of the next query: the shared reqwest client.
#[cfg(not(test))]
pub(crate) fn wire() -> Result<Wire> {
    Ok(Wire::Network(ReqwestTransport(crate::model::client()?)))
}

/// The cache of the client: disk under the store, the shared namespace.
pub fn cache(store_dir: &std::path::Path) -> HttpCache {
    HttpCache::for_store(store_dir, NAMESPACE, MIN_INTERVAL)
}
