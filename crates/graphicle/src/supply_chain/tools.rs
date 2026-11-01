//! The supply-chain tools of plan §4 P3: `deps_of`, `vuln_impact` and
//! `license_of` on the tool surface (api.rs), and the dependencies section a
//! pack lists for its changed files (code_pack.rs).
//!
//! Every answer reads the store's facts first: the lockfile's packages and
//! resolved edges (lock.rs), the import sites that resolve to them
//! (resolve.rs), the OSV advisories and clean scans (osv.rs), and the deps.dev
//! facts an earlier analysis pinned (on_demand.rs). The deps.dev read-through
//! runs only for what the question asks, and only on a miss. When the network
//! cannot answer, the answer names what it could not learn and carries on: an
//! absence is never an error.
//!
//! A citation is the import edge's own `path:line` (the importing file and
//! the line of its `use`), the file list `get_pr_impact` shows, one entry per
//! file. Its summary counts the same things: files, citations and packages.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::lock;
use super::on_demand::{self, Fact, OnDemand, Wire};
use super::resolve::line_of;
use crate::store::{Store, repo, s};
use crate::store_graph::{self, GraphStore};
use crate::{Error, Result, string};

/// The operation of `deps_of`, the code-graph half of the server.
pub const DEPS_OF: &str = "graph.deps_of";
/// The operation of `vuln_impact`.
pub const VULN_IMPACT: &str = "graph.vuln_impact";
/// The operation of `license_of`.
pub const LICENSE_OF: &str = "graph.license_of";

/// The import edges of a symbol, a file or a package, with the package each
/// resolves to. `$key` is the named symbol, file or purl, and the filter
/// names which of the three it is.
const SITES: &str = "MATCH (i:Symbol {repository_id: $repo})-[r:IMPORTS_FROM]->\
    (s:Symbol {repository_id: $repo})-[:RESOLVES_TO]->(pk:Package {repository_id: $repo}) \
    WHERE r.valid_from_episode IS NOT NULL{filter} \
    RETURN pk.purl AS purl, pk.name AS name, s.stable_id AS stable_id, \
    s.qualified_name AS qualified_name, r.relpath AS relpath, \
    r.source_location AS location ORDER BY relpath, location";
/// The file or symbol filters of [`SITES`].
pub(crate) const FROM_FILE: &str = " AND r.relpath = $key";
pub(crate) const FROM_SYMBOL: &str =
    " AND (s.stable_id = $key OR s.qualified_name = $key OR i.stable_id = $key)";
pub(crate) const FROM_PACKAGE: &str = " AND pk.purl = $key";
/// The imports of a file or symbol that name no locked crate.
const UNRESOLVED: &str = "MATCH (u:UnresolvedPackage {repository_id: $repo}) \
    WHERE u.relpath = $key OR u.symbol = $key \
    RETURN u.package AS package, u.use_path AS use_path, u.line AS line, \
    u.reason AS reason ORDER BY package";
/// The packages a name or purl stands for in the lockfile.
const PACKAGES: &str = "MATCH (pk:Package {repository_id: $repo}) \
    WHERE pk.name = $key OR pk.purl = $key RETURN pk.purl AS purl ORDER BY purl";
/// The locked versions of a package, as the lockfile describes them.
const VERSIONS: &str = "MATCH (pk:Package {repository_id: $repo, purl: $key})-[:VERSION]->\
    (pv:PackageVersion {repository_id: $repo})<-[:DESCRIBES]-\
    (:File {repository_id: $repo, path: $file}) \
    RETURN pv.purl_at_version AS key, pv.version AS version, pv.source_kind AS source_kind \
    ORDER BY key";
/// The resolved dependencies of one locked version, as the lockfile says.
const REQUIRES: &str = "MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: $key})\
    -[:REQUIRES {origin: 'cargo.lock'}]->(t:PackageVersion {repository_id: $repo}) \
    RETURN t.name AS name, t.version AS version ORDER BY name, version";
/// The advisories OSV reported for one locked version, with the fixed version
/// each names for it.
pub(crate) const ADVISORIES: &str = "MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: $key})\
    -[r:AFFECTED_BY]->(a:Advisory {repository_id: $repo}) \
    RETURN a.osv_id AS osv_id, a.title AS title, a.url AS url, r.fixed_in AS fixed_in \
    ORDER BY osv_id";
/// OSV's clean scan of one locked version, when it found nothing.
const CLEAN: &str = "MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: $key})\
    -[r:SCANNED_CLEAN]->(:AdvisoryCorpus {repository_id: $repo}) \
    RETURN r.as_of AS as_of, r.source AS source";
/// Whether OSV has scanned this project's lockfile at all.
pub(crate) const SCANNED: &str = "MATCH (c:AdvisoryCorpus {repository_id: $repo, source: 'osv'}) \
    RETURN count(*) AS n";
/// One advisory the project's OSV scans hold.
const ADVISORY: &str = "MATCH (a:Advisory {repository_id: $repo, osv_id: $key}) \
    RETURN a.osv_id AS osv_id, a.title AS title, a.url AS url, a.aliases AS aliases";
/// The locked versions an advisory affects.
const AFFECTED: &str = "MATCH (f:File {repository_id: $repo, path: $file})\
    -[:DESCRIBES]->(pv:PackageVersion {repository_id: $repo})\
    -[:AFFECTED_BY]->(a:Advisory {repository_id: $repo, osv_id: $key}) \
    RETURN pv.purl_at_version AS key, pv.name AS name, pv.version AS version ORDER BY key";
/// The license expressions pinned for one version, with their `as_of`.
const PINNED: &str = "MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: $key})\
    -[r:LICENSED_AS {origin: 'deps.dev'}]->(:License) RETURN r.as_of AS as_of";
/// The manifest requirements pinned for one version, with their `as_of`.
const PINNED_REQUIRES: &str = "MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: $key})\
    -[r:REQUIRES {origin: 'deps.dev'}]->(:PackageVersion {repository_id: $repo}) \
    RETURN r.as_of AS as_of";

/// The one field a call names, of `fields`, and its value.
fn named<'a>(args: &'a Value, fields: &[&'static str]) -> Result<(&'static str, &'a str)> {
    let given: Vec<(&'static str, &'a str)> = fields
        .iter()
        .filter_map(|field| {
            args.get(*field)
                .and_then(Value::as_str)
                .map(|value| (*field, value))
        })
        .collect();
    match given.as_slice() {
        [(field, value)] if !value.trim().is_empty() && value.len() <= 2000 => Ok((*field, *value)),
        _ => Err(Error::bad(format!(
            "Name exactly one of {}",
            fields.join(", ")
        ))),
    }
}

/// The project a call reads, authorized as a read.
fn project(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<String> {
    let named_project = string(args, "repository_id", 100)?;
    store.authorized_read(workspace, token, "read", |db, p| {
        Ok(s(&repo(db, p, named_project)?, "id").to_string())
    })
}

/// One import site that resolves to a locked package: the importing file and
/// line, and the imported name.
#[derive(Debug, Clone, PartialEq)]
pub struct Site {
    pub path: String,
    pub line: Option<i64>,
    pub symbol: String,
    pub purl: String,
    pub name: String,
}

impl Site {
    /// `path:line`, or the path alone when the edge carries no line.
    pub fn citation(&self) -> String {
        citation(&self.path, self.line)
    }

    fn json(&self) -> Value {
        json!({"citation": self.citation(), "path": self.path, "line": self.line,
            "symbol": self.symbol})
    }
}

/// `path:line`, or the path alone.
pub fn citation(path: &str, line: Option<i64>) -> String {
    match line {
        Some(line) => format!("{path}:{line}"),
        None => path.to_owned(),
    }
}

/// The import sites in `scope` (a filter of [`SITES`] and its key), or all
/// of them when `scope` is `None`.
pub(crate) fn sites(
    graph: &GraphStore,
    repository: &str,
    scope: Option<(&str, &str)>,
) -> Result<Vec<Site>> {
    let (filter, key) = scope.unwrap_or(("", ""));
    let cypher = SITES.replace("{filter}", filter);
    let mut params = json!({"repo": repository});
    if scope.is_some() {
        params["key"] = json!(key);
    }
    let rows = graph.read(|db| store_graph::rows(db, &cypher, store_graph::params(&params)))?;
    let mut found: Vec<Site> = rows
        .iter()
        .filter_map(|row| {
            Some(Site {
                path: row["relpath"].as_str()?.to_owned(),
                line: line_of(row["location"].as_str().unwrap_or_default()),
                symbol: row["qualified_name"]
                    .as_str()
                    .or_else(|| row["stable_id"].as_str())?
                    .to_owned(),
                purl: row["purl"].as_str()?.to_owned(),
                name: row["name"].as_str()?.to_owned(),
            })
        })
        .collect();
    // By line as a number: the query orders the location text.
    found.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    Ok(found)
}

/// One locked version of a package with the facts the lockfile and OSV hold
/// for it.
#[derive(Debug, Clone, PartialEq)]
pub struct VersionFacts {
    pub key: String,
    pub version: String,
    /// The resolved dependencies, as `name version`.
    pub requires: Vec<String>,
    /// The OSV ids that affect this version.
    pub advisories: Vec<String>,
}

pub(crate) fn rows(graph: &GraphStore, cypher: &str, params: Value) -> Result<Vec<Value>> {
    graph.read(|db| store_graph::rows(db, cypher, store_graph::params(&params)))
}

fn text(row: &Value, field: &str) -> String {
    row[field].as_str().unwrap_or_default().to_owned()
}

/// The locked versions of `purl` in `repository`, with their resolved
/// dependencies and advisories.
pub(crate) fn versions(
    graph: &GraphStore,
    repository: &str,
    purl: &str,
) -> Result<Vec<VersionFacts>> {
    let locked = rows(
        graph,
        VERSIONS,
        json!({"repo": repository, "file": lock::FILE, "key": purl}),
    )?;
    let mut out = Vec::with_capacity(locked.len());
    for row in locked {
        let key = text(&row, "key");
        let requires = rows(graph, REQUIRES, json!({"repo": repository, "key": key}))?
            .iter()
            .map(|dep| format!("{} {}", text(dep, "name"), text(dep, "version")))
            .collect();
        let advisories = rows(graph, ADVISORIES, json!({"repo": repository, "key": key}))?
            .iter()
            .map(|a| text(a, "osv_id"))
            .collect();
        out.push(VersionFacts {
            version: text(&row, "version"),
            key,
            requires,
            advisories,
        });
    }
    Ok(out)
}

/// The package purls a lockfile name or purl stands for.
pub(crate) fn packages_named(
    graph: &GraphStore,
    repository: &str,
    name: &str,
) -> Result<Vec<String>> {
    Ok(
        rows(graph, PACKAGES, json!({"repo": repository, "key": name}))?
            .iter()
            .map(|row| text(row, "purl"))
            .collect(),
    )
}

/// Group sites by their package.
fn by_package(sites: &[Site]) -> BTreeMap<String, Vec<&Site>> {
    let mut grouped: BTreeMap<String, Vec<&Site>> = BTreeMap::new();
    for site in sites {
        grouped.entry(site.purl.clone()).or_default().push(site);
    }
    grouped
}

/// The sites of a file as JSON, one entry per file with its citations.
pub(crate) fn files_json(sites: &[&Site]) -> Vec<Value> {
    let mut files: BTreeMap<&str, Vec<&Site>> = BTreeMap::new();
    for site in sites {
        files.entry(site.path.as_str()).or_default().push(site);
    }
    files
        .into_iter()
        .map(|(path, sites)| {
            let lines: Vec<i64> = sites.iter().filter_map(|s| s.line).collect();
            json!({"path": path, "citation": sites[0].citation(), "lines": lines,
                "uses": sites.iter().map(|s| s.json()).collect::<Vec<_>>()})
        })
        .collect()
}

/// The count of distinct files in `sites`.
fn file_count(sites: &[&Site]) -> usize {
    sites
        .iter()
        .map(|s| s.path.as_str())
        .collect::<BTreeSet<_>>()
        .len()
}

/// Run `work` on a thread of its own, so its deps.dev reads (async) run
/// inside the synchronous tool dispatch, whatever runtime the caller sits in
/// (jobs.rs's `on_own_runtime`).
fn on_network<T: Send>(work: impl FnOnce() -> Result<T> + Send) -> Result<T> {
    std::thread::scope(|scope| {
        scope
            .spawn(work)
            .join()
            .unwrap_or_else(|_| Err(Error::bad("The deps.dev read panicked")))
    })
}

/// A current-thread runtime for one read-through batch.
fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// The deps.dev manifest requirements of each version, or why they are not
/// known.
async fn requirements_of(
    on_demand: &OnDemand<Wire>,
    keys: &[String],
) -> Vec<(String, std::result::Result<Option<Fact>, String>)> {
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let fetched = on_demand.requirements_for(key).await;
        out.push((key.clone(), fetched.map_err(|e| e.message)));
    }
    out
}

/// The newest `as_of` of the requirements pinned for `key`.
fn newest_requirement_pin(
    graph: &GraphStore,
    repository: &str,
    key: &str,
) -> Result<Option<String>> {
    Ok(rows(
        graph,
        PINNED_REQUIRES,
        json!({"repo": repository, "key": key}),
    )?
    .iter()
    .filter_map(|row| row["as_of"].as_str().map(str::to_owned))
    .max())
}

/// The `deps_of` answer: the packages a symbol's or a file's imports resolve
/// to, their locked versions, what each version's lockfile edges say, and the
/// manifest requirements — the newest pin first, the read-through layer
/// (deps.dev) only on a miss.
pub fn deps_of(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    let (field, value) = named(args, &["symbol", "file"])?;
    let repository = project(store, workspace, token, args)?;
    let graph = store.graph();
    let scope = match field {
        "file" => FROM_FILE,
        _ => FROM_SYMBOL,
    };
    let found = sites(&graph, &repository, Some((scope, value)))?;
    let unresolved = rows(
        &graph,
        UNRESOLVED,
        json!({"repo": repository, "key": value}),
    )?;
    let mut not_known = Vec::new();
    let mut packages = Vec::new();
    let mut version_keys = Vec::new();
    for (purl, sites) in by_package(&found) {
        let mut locked = Vec::new();
        for facts in versions(&graph, &repository, &purl)? {
            version_keys.push(facts.key.clone());
            locked.push(facts);
        }
        packages.push((purl, sites, locked));
    }
    // Pinned first, per version: the newest pin answers, and only a version
    // no pin covers is fetched through the read-through layer.
    let mut requirements = BTreeMap::new();
    let mut missing = Vec::new();
    for key in &version_keys {
        let pinned = match newest_requirement_pin(&graph, &repository, key)? {
            Some(as_of) => on_demand::pinned_requirements(&graph, &repository, key, &as_of)?,
            None => None,
        };
        match pinned {
            Some(Fact::Requirements { dependencies, .. }) => {
                requirements.insert(key.clone(), Some(dependencies));
            }
            _ => missing.push(key.clone()),
        }
    }
    if !missing.is_empty() {
        let on_demand = on_demand::on(&store.directory)?;
        let keys = missing;
        let fetched =
            on_network(move || Ok(runtime()?.block_on(requirements_of(&on_demand, &keys))))?;
        for (key, answer) in fetched {
            match answer {
                Ok(Some(Fact::Requirements { dependencies, .. })) => {
                    requirements.insert(key, Some(dependencies));
                }
                Ok(_) => {
                    not_known.push(format!(
                        "requirements of {key} are not known: deps.dev has none for it"
                    ));
                    requirements.insert(key, None);
                }
                Err(message) => {
                    not_known.push(format!(
                        "requirements of {key} are not known: deps.dev was not reached ({message})"
                    ));
                    requirements.insert(key, None);
                }
            }
        }
    }
    let mut version_count = 0;
    let package_json: Vec<Value> = packages
        .iter()
        .map(|(purl, sites, locked)| {
            version_count += locked.len();
            json!({
                "purl": purl,
                "name": sites[0].name,
                "versions": locked.iter().map(|facts| json!({
                    "purl_at_version": facts.key,
                    "version": facts.version,
                    "requires": facts.requires,
                    "requirements": requirements.get(&facts.key).cloned().flatten()
                        .map(|deps| deps.iter().map(requirement_json).collect::<Vec<_>>()),
                })).collect::<Vec<_>>(),
                "cited": sites.iter().map(|s| s.citation()).collect::<Vec<_>>(),
            })
        })
        .collect();
    let all_sites: Vec<&Site> = found.iter().collect();
    if found.is_empty() && unresolved.is_empty() {
        not_known.push(format!(
            "the code graph holds no import of {value} that resolves to a locked package"
        ));
    }
    let unresolved_json: Vec<Value> = unresolved
        .iter()
        .map(|row| {
            json!({"package": text(row, "package"), "use_path": text(row, "use_path"),
                "citation": citation(value, row["line"].as_i64()), "reason": text(row, "reason")})
        })
        .collect();
    Ok(json!({
        "query": {field: value},
        "packages": package_json,
        "unresolved": unresolved_json,
        "files": files_json(&all_sites),
        "summary": {
            "packages": packages.len(),
            "versions": version_count,
            "citations": found.len(),
            "files": file_count(&all_sites),
            "unresolved": unresolved.len(),
        },
        "not_known": not_known,
    }))
}

fn requirement_json(dep: &on_demand::Requirement) -> Value {
    json!({"name": dep.name, "requirement": dep.requirement, "kind": dep.kind,
        "optional": dep.optional, "features": dep.features, "target": dep.target})
}

/// The advisory status of one locked version in a `vuln_impact` answer.
fn version_status(
    graph: &GraphStore,
    repository: &str,
    facts: &VersionFacts,
    scanned: bool,
    not_known: &mut Vec<String>,
) -> Result<Value> {
    let advisories = rows(
        graph,
        ADVISORIES,
        json!({"repo": repository, "key": facts.key}),
    )?;
    let clean = rows(graph, CLEAN, json!({"repo": repository, "key": facts.key}))?;
    let (status, as_of, source) = match (advisories.is_empty(), clean.first()) {
        (false, _) => ("affected", None, None),
        (true, Some(row)) => ("clean", row["as_of"].as_str(), row["source"].as_str()),
        (true, None) => {
            if scanned {
                not_known.push(format!(
                    "OSV has no scan recorded for {}: it is not a crates.io package",
                    facts.key
                ));
            } else {
                not_known.push(format!(
                    "OSV has not scanned this project's lockfile, so advisories for {} are not known",
                    facts.key
                ));
            }
            ("unscanned", None, None)
        }
    };
    Ok(json!({
        "purl_at_version": facts.key,
        "version": facts.version,
        "status": status,
        "advisories": advisories.iter().map(|a| json!({
            "osv_id": text(a, "osv_id"), "title": text(a, "title"), "url": text(a, "url"),
            "fixed_in": a["fixed_in"]
        })).collect::<Vec<_>>(),
        "as_of": as_of,
        "source": source,
    }))
}

/// The `vuln_impact` answer, for an advisory (the two-hop walk of plan §3:
/// advisory, the locked versions it affects, the packages they are, the
/// import sites that resolve to them) or a package (its locked versions'
/// advisories, or the clean scan that found none, and the same sites).
pub fn vuln_impact(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    let (field, value) = named(args, &["advisory", "package"])?;
    let repository = project(store, workspace, token, args)?;
    let graph = store.graph();
    let mut not_known = Vec::new();
    let scanned = rows(&graph, SCANNED, json!({"repo": repository}))?
        .first()
        .and_then(|row| row["n"].as_i64())
        .unwrap_or(0)
        > 0;
    let (advisory, versions_json, found) = if field == "advisory" {
        let row = rows(&graph, ADVISORY, json!({"repo": repository, "key": value}))?;
        let advisory = match row.first() {
            Some(row) => json!({"osv_id": text(row, "osv_id"), "title": row["title"],
                "url": row["url"], "aliases": row["aliases"]}),
            None => {
                not_known.push(format!(
                    "advisory {value} is not in the store: OSV has not reported it for this project's lockfile"
                ));
                Value::Null
            }
        };
        let affected = rows(
            &graph,
            AFFECTED,
            json!({"repo": repository, "file": lock::FILE, "key": value}),
        )?;
        let mut versions_json = Vec::new();
        // Sites are per package, not per version: two locked versions of one
        // package share their import sites, so each distinct purl is read once.
        let mut purls = BTreeSet::new();
        for row in &affected {
            purls.insert(lock::purl(&text(row, "name")));
            versions_json.push(json!({"purl_at_version": text(row, "key"),
                "version": text(row, "version"), "status": "affected", "advisory": value}));
        }
        let mut found = Vec::new();
        for purl in &purls {
            found.extend(sites(&graph, &repository, Some((FROM_PACKAGE, purl)))?);
        }
        if affected.is_empty() && advisory != Value::Null {
            not_known.push(format!(
                "no locked package of this project is affected by {value}"
            ));
        }
        (advisory, versions_json, found)
    } else {
        let purls = packages_named(&graph, &repository, value)?;
        if purls.is_empty() {
            not_known.push(format!("no locked package named {value} in this project"));
        }
        let mut versions_json = Vec::new();
        let mut found = Vec::new();
        for purl in &purls {
            found.extend(sites(&graph, &repository, Some((FROM_PACKAGE, purl)))?);
            for facts in versions(&graph, &repository, purl)? {
                versions_json.push(version_status(
                    &graph,
                    &repository,
                    &facts,
                    scanned,
                    &mut not_known,
                )?);
            }
        }
        (Value::Null, versions_json, found)
    };
    if !scanned && field == "advisory" {
        not_known.push("OSV has not scanned this project's lockfile yet".to_owned());
    }
    let all_sites: Vec<&Site> = found.iter().collect();
    let affected_versions = versions_json
        .iter()
        .filter(|v| v["status"] == "affected")
        .count();
    let clean_versions = versions_json
        .iter()
        .filter(|v| v["status"] == "clean")
        .count();
    Ok(json!({
        "query": {field: value},
        "advisory": advisory,
        "versions": versions_json,
        "files": files_json(&all_sites),
        "summary": {
            "affected": affected_versions,
            "clean": clean_versions,
            "citations": found.len(),
            "files": file_count(&all_sites),
        },
        "not_known": not_known,
    }))
}

/// A version `license_of` answers for, and the import sites that reach it.
struct Target {
    key: String,
    name: String,
    version: String,
    cited: Vec<String>,
}

/// The licenses of one version: a pin at `as_of`, the newest pin when the
/// call names none, else the read-through layer (`license_answer`).
async fn licenses(
    graph: &GraphStore,
    repository: &str,
    on_demand: &OnDemand<Wire>,
    keys: &[String],
    as_of: &str,
) -> Vec<(String, std::result::Result<Vec<String>, String>)> {
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let answer =
            on_demand::license_answer(graph, repository, on_demand, &[key.as_str()], as_of)
                .await
                .map(|answer| {
                    answer
                        .rows
                        .first()
                        .and_then(|row| row["expressions"].as_array())
                        .map(|list| {
                            list.iter()
                                .filter_map(|e| e.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default()
                });
        out.push((key.clone(), answer.map_err(|e| e.message)));
    }
    out
}

/// The newest `as_of` of the licenses pinned for `key`.
fn newest_pin(graph: &GraphStore, repository: &str, key: &str) -> Result<Option<String>> {
    Ok(
        rows(graph, PINNED, json!({"repo": repository, "key": key}))?
            .iter()
            .filter_map(|row| row["as_of"].as_str().map(str::to_owned))
            .max(),
    )
}

/// The `license_of` answer: the license expressions of the package a symbol,
/// a file or a package maps to, per locked version. Pinned facts first; on a
/// miss, the read-through layer (deps.dev) unless the call names an `as_of`
/// it cannot answer. The lockfile itself records no license, so a version
/// neither pinned nor reachable is named as not known.
pub fn license_of(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    let (field, value) = named(args, &["symbol", "file", "package"])?;
    let as_of_asked = args
        .get("as_of")
        .and_then(Value::as_str)
        .filter(|a| !a.is_empty());
    let repository = project(store, workspace, token, args)?;
    let graph = store.graph();
    let mut not_known = Vec::new();
    let mut targets: BTreeMap<String, Target> = BTreeMap::new();
    let mut add = |facts: &VersionFacts, name: &str, cited: Vec<String>| {
        let target = targets.entry(facts.key.clone()).or_insert_with(|| Target {
            key: facts.key.clone(),
            name: name.to_owned(),
            version: facts.version.clone(),
            cited: Vec::new(),
        });
        target.cited.extend(cited);
        target.cited.sort();
        target.cited.dedup();
    };
    if field == "package" {
        for purl in packages_named(&graph, &repository, value)? {
            for facts in versions(&graph, &repository, &purl)? {
                add(&facts, &purl, Vec::new());
            }
        }
    } else {
        let scope = if field == "file" {
            FROM_FILE
        } else {
            FROM_SYMBOL
        };
        for site in sites(&graph, &repository, Some((scope, value)))? {
            for facts in versions(&graph, &repository, &site.purl)? {
                add(&facts, &site.name, vec![site.citation()]);
            }
        }
    }
    if targets.is_empty() {
        not_known.push(format!(
            "no locked package that {field} {value} maps to is in the store"
        ));
    }
    // Pinned first.
    let mut answers: BTreeMap<String, Value> = BTreeMap::new();
    let mut missing: Vec<String> = Vec::new();
    for target in targets.values() {
        let as_of = match as_of_asked {
            Some(asked) => Some(asked.to_owned()),
            None => newest_pin(&graph, &repository, &target.key)?,
        };
        let pinned = match &as_of {
            Some(as_of) => on_demand::pinned_licenses(&graph, &repository, &target.key, as_of)?,
            None => None,
        };
        match (pinned, as_of) {
            (Some(Fact::Licenses { expressions, .. }), Some(as_of)) => {
                answers.insert(
                    target.key.clone(),
                    license_json(target, &expressions, "pinned", Some(&as_of)),
                );
            }
            _ if as_of_asked.is_some() => {
                not_known.push(format!(
                    "no license of {} is pinned at {}, and the read-through layer answers only the current deps.dev data",
                    target.key,
                    as_of_asked.unwrap_or_default()
                ));
                answers.insert(
                    target.key.clone(),
                    license_json(target, &[], "unknown", None),
                );
            }
            _ => missing.push(target.key.clone()),
        }
    }
    // The read-through layer, for what no pin answers.
    if !missing.is_empty() {
        let on_demand = on_demand::on(&store.directory)?;
        let keys = missing.clone();
        let as_of = crate::now();
        let graph_for_read = graph.clone();
        let repo_id = repository.clone();
        let fetched = on_network(move || {
            Ok(runtime()?.block_on(licenses(
                &graph_for_read,
                &repo_id,
                &on_demand,
                &keys,
                &as_of,
            )))
        })?;
        for (key, answer) in fetched {
            let target = &targets[&key];
            match answer {
                Ok(expressions) if !expressions.is_empty() => {
                    answers.insert(
                        key.clone(),
                        license_json(target, &expressions, "read-through", Some(&crate::now())),
                    );
                }
                Ok(_) => {
                    not_known.push(format!(
                        "deps.dev has no entry for {key}: its license is not known"
                    ));
                    answers.insert(key.clone(), license_json(target, &[], "unknown", None));
                }
                Err(message) => {
                    not_known.push(format!(
                        "the license of {key} is not known: no pin, and deps.dev was not reached ({message})"
                    ));
                    answers.insert(key.clone(), license_json(target, &[], "unknown", None));
                }
            }
        }
    }
    let count = |source: &str| answers.values().filter(|a| a["source"] == source).count();
    let summary = json!({
        "versions": targets.len(),
        "pinned": count("pinned"),
        "read_through": count("read-through"),
        "unknown": count("unknown"),
    });
    let licenses: Vec<Value> = answers.into_values().collect();
    Ok(json!({
        "query": {field: value},
        "licenses": licenses,
        "summary": summary,
        "not_known": not_known,
    }))
}

fn license_json(
    target: &Target,
    expressions: &[String],
    source: &str,
    as_of: Option<&str>,
) -> Value {
    json!({
        "purl_at_version": target.key,
        "name": target.name,
        "version": target.version,
        "expressions": expressions,
        "origin": if source == "unknown" { Value::Null } else { json!(on_demand::ORIGIN) },
        "source": source,
        "as_of": as_of,
        "cited": target.cited,
    })
}

/// What a pack's dependencies section lists for one package a file imports.
#[derive(Debug, Clone, PartialEq)]
pub struct Dependency {
    pub name: String,
    pub line: Option<i64>,
    pub versions: Vec<VersionFacts>,
}

/// The packages each file's imports resolve to, with their locked versions'
/// dependencies and advisories: the pack's dependencies section reads it for
/// its changed files. Offline: the lockfile and OSV facts only.
pub fn dependencies_by_file(
    graph: &GraphStore,
    repository: &str,
) -> Result<BTreeMap<String, Vec<Dependency>>> {
    let found = sites(graph, repository, None)?;
    let mut cache: BTreeMap<String, Vec<VersionFacts>> = BTreeMap::new();
    let mut out: BTreeMap<String, Vec<Dependency>> = BTreeMap::new();
    for site in &found {
        if !cache.contains_key(&site.purl) {
            cache.insert(site.purl.clone(), versions(graph, repository, &site.purl)?);
        }
        let list = out.entry(site.path.clone()).or_default();
        match list.iter_mut().find(|d| d.name == site.name) {
            Some(dependency) => {
                dependency.line = dependency.line.or(site.line);
            }
            None => list.push(Dependency {
                name: site.name.clone(),
                line: site.line,
                versions: cache[&site.purl].clone(),
            }),
        }
    }
    Ok(out)
}

/// One line of a pack's dependencies section for a file: the packages it
/// imports, their locked versions, what those depend on, and the advisories
/// that affect them.
pub fn dependency_lines(path: &str, dependencies: &[Dependency]) -> String {
    let mut text = String::new();
    for dependency in dependencies {
        for facts in &dependency.versions {
            let at = dependency
                .line
                .map_or(String::new(), |line| format!(", imported at {path}:{line}"));
            let requires = if facts.requires.is_empty() {
                "none".to_owned()
            } else {
                facts.requires.join(", ")
            };
            let advisories = if facts.advisories.is_empty() {
                "none".to_owned()
            } else {
                facts.advisories.join(", ")
            };
            text.push_str(&format!(
                "- {} {}{at}; requires: {requires}; advisories: {advisories}\n",
                dependency.name, facts.version
            ));
        }
    }
    text
}
