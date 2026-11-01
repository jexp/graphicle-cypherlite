//! The on-the-fly supply-chain layer (plan §4 P2): deps.dev is a
//! *read-through* source, never an eager job. An analysis, a tool or a pack
//! calls the [`OnDemand`] surface when it needs a fact — a version's
//! licenses, a project's Scorecard, the repository a version ships from, a
//! deprecation, a manifest requirement — and the call serves from the HTTP
//! cache, fetching through the shared throttle only on a miss. Nothing on
//! the read path writes to the store: nothing is materialized that no
//! answer asked for, no ecosystem-wide fetch exists anywhere, and there is
//! no deps.dev job kind to enqueue — a plain sync of a lockfile-carrying
//! project performs zero deps.dev requests, because the sync path never
//! calls this module. OSV stays the one eager exception (§4 P2,
//! [`super::osv`]): one bulk query per lockfile digest, still the only
//! supply-chain job a sync queues.
//!
//! # The pin write path
//!
//! The store grows only through [`pin`], when an answer must outlive the
//! question — a report, a CI verdict, an answer with evidence. The facts
//! the answer consumed are written with `origin` ('deps.dev'), `known_at`,
//! `as_of` and the analysis reference, so a pinned analysis is re-runnable
//! against the pinned data: its answer function ([`license_answer`] is the
//! pattern's exemplar) reads pinned facts first, matching `as_of`, and only
//! falls through to the read-through layer when the pin is absent.
//!
//! The model shapes follow plan §2. Licenses are
//! `(:PackageVersion)-[:LICENSED_AS {expression}]->(:License {spdx})` — one
//! `:License` node per distinct SPDX expression, so the licenses of many
//! versions dedupe onto shared nodes; a version deps.dev lists no license
//! for pins `NOASSERTION`, SPDX's "no license known", so the negative
//! answer is a citable fact like a `SCANNED_CLEAN` edge, not an absence.
//! Scorecard, source repository and deprecation are origin-tagged
//! properties on the version the analysis asked about, each family with its
//! own `origin`/`known_at`/`as_of`/`analysis`, because families pin at
//! different times. A pinned manifest requirement joins onto the store's
//! `REQUIRES` edges with origin 'deps.dev', carrying the
//! requirement/kind/optional/features/target the lockfile cannot say — the
//! enrichment gap plan §4 P1 deliberately deferred to this layer.
//!
//! Pins are evidence, not sync state. A license edge is keyed by its
//! expression *and* `as_of`, so pins accumulate rather than replace each
//! other — two pins at the same `as_of` that saw different worlds union on
//! the license edges (every expression either one named), while a property
//! family keeps the last pin's values. A re-run whose `as_of` no longer
//! matches finds no pin and falls through to the read-through layer —
//! pinned only when, and as long as, the answer needs it.

use crate::store_graph::{self, GraphStore};
use crate::supply_chain::deps_dev::{self, Client};
use crate::supply_chain::http_cache::{FetchError, HttpCache, HttpResponse, Transport};
use crate::{Error, Result};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;

#[cfg(not(test))]
use crate::supply_chain::http_cache::ReqwestTransport;
#[cfg(test)]
use crate::supply_chain::http_cache::test_transport::FixtureTransport;
#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::sync::Arc;

/// The provenance tag on every fact this module pins (plan §2's `origin`).
pub const ORIGIN: &str = "deps.dev";

/// The SPDX id a version with no known license pins: the negative answer is
/// a fact, not an absence (the `SCANNED_CLEAN` discipline).
pub const NO_LICENSE: &str = "NOASSERTION";

/// One manifest dependency as deps.dev reports it (the `:requirements`
/// answer): the half of a dependency the lockfile cannot carry. `name` is
/// the crate name — `package =` renames applied — because that is the name
/// the lockfile's resolved edge is joined by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    pub name: String,
    /// The version constraint as the manifest wrote it (`^0.4.2`).
    pub requirement: String,
    /// The table it came from (`normal`, `dev`, `build`).
    pub kind: String,
    /// Whether the manifest marks it optional.
    pub optional: bool,
    /// The features it turns on.
    pub features: Vec<String>,
    /// The platform cfg it is bound to, empty when there is none.
    pub target: String,
}

/// One deps.dev fact as an analysis consumed it: the unit the read-through
/// surface returns and [`pin`] writes. Every variant names the version it
/// is about, because a pin is written on that version's node.
#[derive(Debug, Clone, PartialEq)]
pub enum Fact {
    /// The SPDX expressions of one version, sorted, [`NO_LICENSE`] when
    /// deps.dev lists none.
    Licenses {
        purl_at_version: String,
        expressions: Vec<String>,
    },
    /// Whether the package is deprecated at this version, and the reason
    /// deps.dev gives. deps.dev flags deprecation per version and carries
    /// the reason only in the package's version list, so this fact is the
    /// package's answer for the locked version.
    Deprecation {
        purl_at_version: String,
        deprecated: bool,
        reason: Option<String>,
    },
    /// The repository a version ships from (its `SOURCE_REPO` link).
    SourceRepo {
        purl_at_version: String,
        url: String,
    },
    /// The OpenSSF Scorecard of the project a version ships from: the
    /// stars, the overall score and the date the scan ran. `project` is the
    /// repository path deps.dev keys the project by.
    Scorecard {
        purl_at_version: String,
        project: String,
        stars: u64,
        overall: f64,
        scored_at: Option<String>,
    },
    /// The manifest constraints of one version: what its `Cargo.toml` asked
    /// for, which the lockfile's resolved edges do not say.
    Requirements {
        purl_at_version: String,
        dependencies: Vec<Requirement>,
    },
}

impl Fact {
    /// The `purl_at_version` the fact is about: the node a pin writes it on.
    pub fn key(&self) -> &str {
        match self {
            Self::Licenses {
                purl_at_version, ..
            }
            | Self::Deprecation {
                purl_at_version, ..
            }
            | Self::SourceRepo {
                purl_at_version, ..
            }
            | Self::Scorecard {
                purl_at_version, ..
            }
            | Self::Requirements {
                purl_at_version, ..
            } => purl_at_version,
        }
    }
}

/// What one answer run produced: the cited rows it gives, and the facts it
/// consumed — the input of [`pin`] when the answer must outlive the
/// question.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub rows: Vec<Value>,
    pub consumed: Vec<Fact>,
}

/// The read-through surface: what an analysis, a tool or a pack calls when
/// it needs a deps.dev fact. Every call serves from the [`HttpCache`] under
/// the client and fetches through the shared throttle only on a miss; none
/// of them writes to the store. A purl deps.dev has no entry for is
/// `Ok(None)` — no fact — and a failed request is an error, so nothing
/// partial is ever pinned.
pub struct OnDemand<T: Transport> {
    client: Client<T>,
}

impl<T: Transport> OnDemand<T> {
    /// A read-through layer over `transport` and its cache (the deps.dev
    /// client's own: [`cache`] builds it in production).
    pub fn new(transport: T, cache: HttpCache) -> Self {
        Self {
            client: Client::new(transport, cache),
        }
    }

    /// The SPDX expressions of one version, sorted for a deterministic
    /// citation. A version deps.dev lists no license for answers
    /// [`NO_LICENSE`], so the pin has an edge to write and the answer is
    /// re-runnable.
    pub async fn licenses_for(&self, purl_at_version: &str) -> Result<Option<Fact>> {
        let Some(version) = self.version(purl_at_version).await? else {
            return Ok(None);
        };
        let expressions = if version.licenses.is_empty() {
            vec![NO_LICENSE.to_owned()]
        } else {
            version
                .licenses
                .into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        };
        Ok(Some(Fact::Licenses {
            purl_at_version: purl_at_version.to_owned(),
            expressions,
        }))
    }

    /// The deprecation of the package at this version, with the reason
    /// deps.dev gives. The reason lives only in the package's version list,
    /// so this is the package fetch (`deprecation_for(package)` of the
    /// plan's P2 list), not the version's.
    pub async fn deprecation_for(&self, purl_at_version: &str) -> Result<Option<Fact>> {
        let key = self.key(purl_at_version)?;
        let package = match self.client.package(&key.system, &key.name).await {
            Ok(package) => package,
            Err(error) => return fetch(error),
        };
        // The package's list names every version; ours is the locked one.
        let Some(entry) = package
            .versions
            .into_iter()
            .find(|entry| entry.version_key.version == key.version)
        else {
            return Ok(None);
        };
        Ok(Some(Fact::Deprecation {
            purl_at_version: purl_at_version.to_owned(),
            deprecated: entry.is_deprecated,
            reason: entry.deprecated_reason,
        }))
    }

    /// The repository a version ships from: its `SOURCE_REPO` link. The
    /// link names a URL; [`project_id`] turns it into the project key the
    /// Scorecard is asked by.
    pub async fn source_repo_for(&self, purl_at_version: &str) -> Result<Option<Fact>> {
        let Some(version) = self.version(purl_at_version).await? else {
            return Ok(None);
        };
        let url = version
            .links
            .iter()
            .find(|link| link.label == "SOURCE_REPO")
            .map(|link| link.url.clone());
        Ok(url.map(|url| Fact::SourceRepo {
            purl_at_version: purl_at_version.to_owned(),
            url,
        }))
    }

    /// The OpenSSF Scorecard of the project a version ships from, pinned to
    /// the version the analysis asked about. `project` is the repository
    /// path deps.dev keys the project by (`github.com/time-rs/time`) —
    /// usually [`project_id`] of the version's source repository.
    pub async fn scorecard_for(
        &self,
        purl_at_version: &str,
        project: &str,
    ) -> Result<Option<Fact>> {
        let record = match self.client.project(project).await {
            Ok(record) => record,
            Err(error) => return fetch(error),
        };
        let Some(scorecard) = record.scorecard else {
            return Ok(None);
        };
        Ok(Some(Fact::Scorecard {
            purl_at_version: purl_at_version.to_owned(),
            project: record.project_key.id,
            stars: record.stars_count,
            overall: scorecard.overall_score,
            scored_at: scorecard.date.map(|date| date.to_rfc3339()),
        }))
    }

    /// The manifest constraints of one version: what its `Cargo.toml` asked
    /// for — the requirement, the table, the optional flag, the features,
    /// the target cfg — none of which the lockfile can say. Only the Cargo
    /// shape is typed; another ecosystem's manifest is no fact yet.
    pub async fn requirements_for(&self, purl_at_version: &str) -> Result<Option<Fact>> {
        let key = self.key(purl_at_version)?;
        let requirements = match self.client.requirements(&key).await {
            Ok(requirements) => requirements,
            Err(error) => return fetch(error),
        };
        let Some(cargo) = requirements.cargo else {
            return Ok(None);
        };
        let dependencies = cargo
            .dependencies
            .iter()
            .map(|dep| Requirement {
                // `package =` renames: the lockfile resolves the crate name,
                // so that is the name the join looks for.
                name: if dep.package_alias.is_empty() {
                    dep.name.clone()
                } else {
                    dep.package_alias.clone()
                },
                requirement: dep.requirement.clone(),
                kind: dep.kind.clone(),
                optional: dep.optional,
                features: dep.features.clone(),
                target: dep.target.clone(),
            })
            .collect();
        Ok(Some(Fact::Requirements {
            purl_at_version: purl_at_version.to_owned(),
            dependencies,
        }))
    }

    /// One version's record, `None` when deps.dev has no entry for the purl.
    async fn version(&self, purl_at_version: &str) -> Result<Option<deps_dev::Version>> {
        let key = self.key(purl_at_version)?;
        match self.client.version(&key).await {
            Ok(version) => Ok(Some(version)),
            Err(error) => fetch(error),
        }
    }

    /// The deps.dev key of a `purl_at_version` (the store's own key: a purl).
    fn key(&self, purl_at_version: &str) -> Result<deps_dev::VersionKey> {
        deps_dev::version_key(purl_at_version).map_err(|error| Error::bad(error.to_string()))
    }
}

/// A deps.dev call's answer as this module returns it: a 404 is no fact
/// (`None`), anything else is a failure.
fn fetch<T>(error: deps_dev::Error) -> Result<Option<T>> {
    match error {
        deps_dev::Error::NotFound { .. } => Ok(None),
        error => Err(Error::bad(format!("deps.dev request failed: {error}"))),
    }
}

/// The deps.dev project id of a repository URL: the scheme and any trailing
/// slash dropped, as the `SOURCE_REPO` link needs to become the project
/// key (`https://github.com/time-rs/time` → `github.com/time-rs/time`).
pub fn project_id(repo_url: &str) -> String {
    repo_url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_owned()
}

/// Write the facts one analysis consumed (plan §4 P2's pin write path):
/// every fact lands with `origin` 'deps.dev', `known_at`, `as_of` and the
/// `analysis` reference, so the analysis is re-runnable against the pinned
/// data. Facts about a version the store does not hold, and manifest
/// constraints the lockfile resolved no version for, are reported in the
/// result — never dropped silently. Returns the pin's report.
pub fn pin(
    graph: &GraphStore,
    repository_id: &str,
    facts: &[Fact],
    analysis: &str,
    known_at: &str,
    as_of: &str,
) -> Result<Value> {
    // A pin is written on the version's node, so the versions the store
    // holds decide what can be pinned.
    let keys: Vec<Value> = facts
        .iter()
        .map(|fact| fact.key().to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|key| json!(key))
        .collect();
    let known = versions(graph, repository_id, &keys)?;
    // The lockfile's resolved edges, as the requirements join looks for them.
    let resolved: BTreeSet<(String, String)> = graph
        .read(|db| {
            store_graph::rows(
                db,
                "UNWIND $keys AS key \
                 MATCH (v:PackageVersion {repository_id: $repo, purl_at_version: key})\
                 -[:REQUIRES {origin: 'cargo.lock'}]->(t:PackageVersion {repository_id: $repo}) \
                 RETURN key AS key, t.name AS name",
                store_graph::params(&json!({"repo": repository_id, "keys": keys})),
            )
        })?
        .into_iter()
        .filter_map(|row| {
            Some((
                row["key"].as_str()?.to_owned(),
                row["name"].as_str()?.to_owned(),
            ))
        })
        .collect();

    let mut unmatched: BTreeSet<String> = BTreeSet::new();
    let mut licenses: Vec<Value> = Vec::new();
    let mut deprecations: Vec<Value> = Vec::new();
    let mut source_repos: Vec<Value> = Vec::new();
    let mut scorecards: Vec<Value> = Vec::new();
    let mut requires: Vec<Value> = Vec::new();
    let mut unjoined: BTreeSet<String> = BTreeSet::new();
    let (mut license_facts, mut deprecation_facts, mut source_repo_facts, mut scorecard_facts) =
        (0usize, 0usize, 0usize, 0usize);
    let mut joined = 0usize;
    for fact in facts {
        let key = fact.key();
        if !known.contains(key) {
            unmatched.insert(key.to_owned());
            continue;
        }
        // Every row carries the stamps: the batches helper passes rows and
        // the project, nothing else.
        let stamped = |mut row: Value| {
            row["known_at"] = json!(known_at);
            row["as_of"] = json!(as_of);
            row["analysis"] = json!(analysis);
            row
        };
        match fact {
            Fact::Licenses { expressions, .. } => {
                license_facts += 1;
                for expression in expressions {
                    licenses.push(stamped(json!({"key": key, "expression": expression})));
                }
            }
            Fact::Deprecation {
                deprecated, reason, ..
            } => {
                deprecation_facts += 1;
                deprecations.push(stamped(json!({"key": key, "deprecated": deprecated,
                    "reason": reason})));
            }
            Fact::SourceRepo { url, .. } => {
                source_repo_facts += 1;
                source_repos.push(stamped(json!({"key": key, "url": url})));
            }
            Fact::Scorecard {
                project,
                stars,
                overall,
                scored_at,
                ..
            } => {
                scorecard_facts += 1;
                scorecards.push(stamped(json!({"key": key, "project": project,
                    "stars": stars, "overall": overall, "scored_at": scored_at})));
            }
            Fact::Requirements {
                dependencies,
                purl_at_version,
            } => {
                for dependency in dependencies {
                    if resolved.contains(&(purl_at_version.to_owned(), dependency.name.clone())) {
                        joined += 1;
                        // Features are a list; the store's rule for dynamic
                        // values is a stringified JSON property (osv's aliases).
                        requires.push(stamped(json!({"key": purl_at_version,
                            "crate": dependency.name,
                            "requirement": dependency.requirement, "kind": dependency.kind,
                            "optional": dependency.optional,
                            "features": serde_json::to_string(&dependency.features)
                                .unwrap_or_default(),
                            "target": dependency.target})));
                    } else {
                        // A dev- or disabled-optional constraint the lockfile
                        // resolved no version for: reported, never dropped.
                        unjoined.insert(dependency.name.clone());
                    }
                }
            }
        }
    }

    super::batches(
        graph,
        repository_id,
        &licenses,
        "UNWIND $rows AS row \
         MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: row.key}) \
         MERGE (l:License {repository_id: $repo, spdx: row.expression}) \
         MERGE (pv)-[r:LICENSED_AS {expression: row.expression, as_of: row.as_of}]->(l) \
         SET r.origin = 'deps.dev', r.known_at = row.known_at, r.analysis = row.analysis",
    )?;
    super::batches(
        graph,
        repository_id,
        &deprecations,
        "UNWIND $rows AS row \
         MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: row.key}) \
         SET pv.is_deprecated = row.deprecated, pv.deprecated_reason = row.reason, \
             pv.deprecation_origin = 'deps.dev', pv.deprecation_known_at = row.known_at, \
             pv.deprecation_as_of = row.as_of, pv.deprecation_analysis = row.analysis",
    )?;
    super::batches(
        graph,
        repository_id,
        &source_repos,
        "UNWIND $rows AS row \
         MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: row.key}) \
         SET pv.source_repo = row.url, pv.source_repo_origin = 'deps.dev', \
             pv.source_repo_known_at = row.known_at, pv.source_repo_as_of = row.as_of, \
             pv.source_repo_analysis = row.analysis",
    )?;
    super::batches(
        graph,
        repository_id,
        &scorecards,
        "UNWIND $rows AS row \
         MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: row.key}) \
         SET pv.scorecard_project = row.project, pv.scorecard_stars = row.stars, \
             pv.scorecard_overall = row.overall, pv.scorecard_date = row.scored_at, \
             pv.scorecard_origin = 'deps.dev', pv.scorecard_known_at = row.known_at, \
             pv.scorecard_as_of = row.as_of, pv.scorecard_analysis = row.analysis",
    )?;
    super::batches(
        graph,
        repository_id,
        &requires,
        "UNWIND $rows AS row \
         MATCH (v:PackageVersion {repository_id: $repo, purl_at_version: row.key}) \
         MATCH (v)-[:REQUIRES {origin: 'cargo.lock'}]->(t:PackageVersion {repository_id: $repo}) \
         WHERE t.name = row.crate \
         MERGE (v)-[r:REQUIRES {origin: 'deps.dev'}]->(t) \
         SET r.requirement = row.requirement, r.kind = row.kind, r.optional = row.optional, \
             r.features = row.features, r.target = row.target, \
             r.known_at = row.known_at, r.as_of = row.as_of, r.analysis = row.analysis",
    )?;

    Ok(json!({
        "origin": ORIGIN, "analysis": analysis, "known_at": known_at, "as_of": as_of,
        "licenses": license_facts, "deprecations": deprecation_facts,
        "source_repos": source_repo_facts, "scorecards": scorecard_facts,
        "requires_joined": joined, "requires_unjoined": unjoined.into_iter()
            .collect::<Vec<_>>(),
        "unmatched": unmatched.into_iter().collect::<Vec<_>>(),
    }))
}

/// The versions of `keys` the store holds: what a pin can be written on.
fn versions(graph: &GraphStore, repository_id: &str, keys: &[Value]) -> Result<BTreeSet<String>> {
    Ok(graph
        .read(|db| {
            store_graph::rows(
                db,
                "UNWIND $keys AS key \
                 MATCH (:PackageVersion {repository_id: $repo, purl_at_version: key}) \
                 RETURN key AS key",
                store_graph::params(&json!({"repo": repository_id, "keys": keys})),
            )
        })?
        .into_iter()
        .filter_map(|row| row["key"].as_str().map(str::to_owned))
        .collect())
}

/// The answer of a `license_of`-shaped analysis (plan §4 P3's tool, seeded
/// here as the pin pattern's exemplar): one cited row per version asked
/// about. A pinned analysis re-runs it with the same `as_of`: a version
/// whose pin matches answers from the store, and only a version with no
/// such pin is fetched through the read-through layer — so re-running a
/// pinned report reproduces the same citations without the network, whatever
/// the world has changed into since. The rows are the citations; what the
/// run consumed ([`Answer::consumed`]) is what [`pin`] writes when the
/// answer must outlive the question.
pub async fn license_answer<T: Transport>(
    graph: &GraphStore,
    repository_id: &str,
    on_demand: &OnDemand<T>,
    purls: &[&str],
    as_of: &str,
) -> Result<Answer> {
    let mut rows = Vec::with_capacity(purls.len());
    let mut consumed = Vec::with_capacity(purls.len());
    for purl in purls {
        // The pin first, matched on as_of; the read-through only when the
        // pin is absent.
        let fact = match pinned_licenses(graph, repository_id, purl, as_of)? {
            Some(pinned) => pinned,
            None => match on_demand.licenses_for(purl).await? {
                Some(fetched) => fetched,
                None => {
                    // deps.dev has no entry for the purl: the row says so,
                    // and nothing was consumed to pin.
                    rows.push(json!({"purl_at_version": purl, "expressions": [],
                        "origin": ORIGIN, "as_of": as_of}));
                    continue;
                }
            },
        };
        let Fact::Licenses { expressions, .. } = &fact else {
            return Err(Error::bad("a licenses read answered another fact"));
        };
        rows.push(json!({"purl_at_version": purl, "expressions": expressions,
            "origin": ORIGIN, "as_of": as_of}));
        consumed.push(fact);
    }
    Ok(Answer { rows, consumed })
}

/// The licenses pinned for a version at `as_of`, as the fact that was
/// consumed — `None` when no pin matches, which is the caller's fall-through
/// to the read-through layer.
pub fn pinned_licenses(
    graph: &GraphStore,
    repository_id: &str,
    purl_at_version: &str,
    as_of: &str,
) -> Result<Option<Fact>> {
    let expressions = graph
        .read(|db| {
            store_graph::rows(
                db,
                "MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: $key})\
                 -[r:LICENSED_AS {origin: 'deps.dev'}]->(:License) \
                 WHERE r.as_of = $as_of \
                 RETURN r.expression AS expression ORDER BY expression",
                store_graph::params(&json!({"repo": repository_id, "key": purl_at_version,
                    "as_of": as_of})),
            )
        })?
        .into_iter()
        .filter_map(|row| row["expression"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    Ok((!expressions.is_empty()).then(|| Fact::Licenses {
        purl_at_version: purl_at_version.to_owned(),
        expressions,
    }))
}

/// The deprecation pinned for a version at `as_of`, as the fact that was
/// consumed ([`pinned_licenses`]'s contract).
pub fn pinned_deprecation(
    graph: &GraphStore,
    repository_id: &str,
    purl_at_version: &str,
    as_of: &str,
) -> Result<Option<Fact>> {
    let row = graph.read(|db| {
        store_graph::one(
            db,
            "MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: $key}) \
             WHERE pv.deprecation_origin = 'deps.dev' AND pv.deprecation_as_of = $as_of \
             RETURN pv.is_deprecated AS deprecated, pv.deprecated_reason AS reason",
            store_graph::params(&json!({"repo": repository_id, "key": purl_at_version,
                "as_of": as_of})),
        )
    })?;
    Ok(row.map(|row| Fact::Deprecation {
        purl_at_version: purl_at_version.to_owned(),
        deprecated: row["deprecated"] == true,
        reason: row["reason"].as_str().map(str::to_owned),
    }))
}

/// The source repository pinned for a version at `as_of`, as the fact that
/// was consumed ([`pinned_licenses`]'s contract).
pub fn pinned_source_repo(
    graph: &GraphStore,
    repository_id: &str,
    purl_at_version: &str,
    as_of: &str,
) -> Result<Option<Fact>> {
    let row = graph.read(|db| {
        store_graph::one(
            db,
            "MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: $key}) \
             WHERE pv.source_repo_origin = 'deps.dev' AND pv.source_repo_as_of = $as_of \
             RETURN pv.source_repo AS url",
            store_graph::params(&json!({"repo": repository_id, "key": purl_at_version,
                "as_of": as_of})),
        )
    })?;
    Ok(row.and_then(|row| {
        row["url"].as_str().map(|url| Fact::SourceRepo {
            purl_at_version: purl_at_version.to_owned(),
            url: url.to_owned(),
        })
    }))
}

/// The Scorecard pinned for a version at `as_of`, as the fact that was
/// consumed ([`pinned_licenses`]'s contract).
pub fn pinned_scorecard(
    graph: &GraphStore,
    repository_id: &str,
    purl_at_version: &str,
    as_of: &str,
) -> Result<Option<Fact>> {
    let row = graph.read(|db| {
        store_graph::one(
            db,
            "MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: $key}) \
             WHERE pv.scorecard_origin = 'deps.dev' AND pv.scorecard_as_of = $as_of \
             RETURN pv.scorecard_project AS project, pv.scorecard_stars AS stars, \
                    pv.scorecard_overall AS overall, pv.scorecard_date AS scored_at",
            store_graph::params(&json!({"repo": repository_id, "key": purl_at_version,
                "as_of": as_of})),
        )
    })?;
    Ok(row.and_then(|row| {
        Some(Fact::Scorecard {
            purl_at_version: purl_at_version.to_owned(),
            project: row["project"].as_str()?.to_owned(),
            stars: row["stars"].as_u64()?,
            overall: row["overall"].as_f64()?,
            scored_at: row["scored_at"].as_str().map(str::to_owned),
        })
    }))
}

/// The manifest requirements pinned for a version at `as_of`, as the fact
/// that was consumed: the constraints the lockfile could resolve, joined
/// onto its `REQUIRES` edges. A constraint the lockfile resolved no version
/// for is not among them — it was reported, not pinned, when [`pin`] wrote.
pub fn pinned_requirements(
    graph: &GraphStore,
    repository_id: &str,
    purl_at_version: &str,
    as_of: &str,
) -> Result<Option<Fact>> {
    let rows = graph.read(|db| {
        store_graph::rows(
            db,
            "MATCH (v:PackageVersion {repository_id: $repo, purl_at_version: $key})\
             -[r:REQUIRES {origin: 'deps.dev'}]->(t:PackageVersion {repository_id: $repo}) \
             WHERE r.as_of = $as_of \
             RETURN t.name AS name, r.requirement AS requirement, r.kind AS kind, \
                    r.optional AS optional, r.features AS features, r.target AS target \
             ORDER BY name",
            store_graph::params(&json!({"repo": repository_id, "key": purl_at_version,
                "as_of": as_of})),
        )
    })?;
    let mut dependencies = Vec::with_capacity(rows.len());
    for row in rows {
        let features: Vec<String> = serde_json::from_str(
            row["features"].as_str().unwrap_or_default(),
        )
        .map_err(|error| Error::bad(format!("a pinned feature list is unreadable: {error}")))?;
        dependencies.push(Requirement {
            name: row["name"].as_str().unwrap_or_default().to_owned(),
            requirement: row["requirement"].as_str().unwrap_or_default().to_owned(),
            kind: row["kind"].as_str().unwrap_or_default().to_owned(),
            optional: row["optional"] == true,
            features,
            target: row["target"].as_str().unwrap_or_default().to_owned(),
        });
    }
    Ok((!dependencies.is_empty()).then(|| Fact::Requirements {
        purl_at_version: purl_at_version.to_owned(),
        dependencies,
    }))
}

/// The transport an on-demand read runs over: the network in production, a
/// fixture (or a stub that panics on any request, to prove a path never
/// fetches) in tests. The osv job's wire pattern; [`on`] is the entry that
/// builds a layer on it.
pub enum Wire {
    #[cfg(not(test))]
    Network(ReqwestTransport),
    #[cfg(test)]
    Fixture(Arc<FixtureTransport>),
    #[cfg(test)]
    Panic(Arc<PanicTransport>),
}

impl Clone for Wire {
    fn clone(&self) -> Self {
        match self {
            #[cfg(not(test))]
            Self::Network(transport) => Self::Network(transport.clone()),
            #[cfg(test)]
            Self::Fixture(transport) => Self::Fixture(transport.clone()),
            #[cfg(test)]
            Self::Panic(transport) => Self::Panic(transport.clone()),
        }
    }
}

impl Transport for Wire {
    async fn get(&self, url: &str) -> std::result::Result<HttpResponse, FetchError> {
        match self {
            #[cfg(not(test))]
            Self::Network(transport) => transport.get(url).await,
            #[cfg(test)]
            Self::Fixture(transport) => transport.get(url).await,
            #[cfg(test)]
            Self::Panic(transport) => transport.get(url).await,
        }
    }

    async fn post(&self, url: &str, body: &str) -> std::result::Result<HttpResponse, FetchError> {
        match self {
            #[cfg(not(test))]
            Self::Network(transport) => transport.post(url, body).await,
            #[cfg(test)]
            Self::Fixture(transport) => transport.post(url, body).await,
            #[cfg(test)]
            Self::Panic(transport) => transport.post(url, body).await,
        }
    }
}

#[cfg(test)]
thread_local! {
    static INSTALLED: RefCell<Option<Wire>> = const { RefCell::new(None) };
}

/// Serve this thread's on-demand reads from `fixture` (the osv fixture
/// pattern).
#[cfg(test)]
pub(crate) fn install_fixture(fixture: Arc<FixtureTransport>) {
    INSTALLED.with(|slot| *slot.borrow_mut() = Some(Wire::Fixture(fixture)));
}

/// Fail this thread's on-demand reads by panicking, and return the counter
/// that reads how many requests reached the stub: the tool for a test that
/// proves a path performs no deps.dev requests — zero, or the test has
/// already panicked.
#[cfg(test)]
pub(crate) fn install_panic() -> Arc<PanicTransport> {
    let transport = Arc::new(PanicTransport::default());
    INSTALLED.with(|slot| *slot.borrow_mut() = Some(Wire::Panic(transport.clone())));
    transport
}

/// The wire of the next on-demand read, built on the calling thread: the
/// installed one, else a stub that panics — a test that forgot its fixture
/// fails loudly instead of reaching the network.
#[cfg(test)]
pub(crate) fn wire() -> Result<Wire> {
    Ok(INSTALLED
        .with(|slot| slot.borrow().clone())
        .unwrap_or_else(|| Wire::Panic(Arc::new(PanicTransport::default()))))
}

/// The wire of the next on-demand read: the shared reqwest client.
#[cfg(not(test))]
pub(crate) fn wire() -> Result<Wire> {
    Ok(Wire::Network(ReqwestTransport(crate::model::client()?)))
}

/// The cache of the read-through layer: disk under the store, the deps.dev
/// namespace, network calls gated by the client's own interval.
pub fn cache(store_dir: &Path) -> HttpCache {
    HttpCache::for_store(store_dir, deps_dev::NAMESPACE, deps_dev::MIN_INTERVAL)
}

/// The read-through layer of the store at `store_dir`: what an analysis,
/// tool or pack holds when it needs a deps.dev fact. The layer is never
/// built on the sync path — that is the offline guarantee.
pub fn on(store_dir: &Path) -> Result<OnDemand<Wire>> {
    Ok(OnDemand::new(wire()?, cache(store_dir)))
}

/// A transport that panics on any request. It names the URL it was asked
/// for, and counts the requests that reached it, so a test can assert zero.
#[cfg(test)]
#[derive(Default)]
pub struct PanicTransport {
    calls: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl PanicTransport {
    /// The number of requests that reached this stub.
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn refuse(&self, url: &str) -> ! {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        panic!("deps.dev was fetched on a path that must stay offline: {url}")
    }
}

#[cfg(test)]
impl Transport for PanicTransport {
    async fn get(&self, url: &str) -> std::result::Result<HttpResponse, FetchError> {
        self.refuse(url)
    }

    async fn post(&self, url: &str, _body: &str) -> std::result::Result<HttpResponse, FetchError> {
        self.refuse(url)
    }
}
