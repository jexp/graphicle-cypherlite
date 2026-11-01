//! `Cargo.lock` into the package model: the reader turns the lockfile into
//! plain structs ([`parse`]), and the writer ([`ingest`]) maps them onto the
//! graph as anchored MERGE batches, the git-history writer's shape
//! (git_history/writer.rs).
//!
//! # What the lockfile can and cannot say
//!
//! `Cargo.lock` lists every resolved package with its exact version, its
//! source (absent for path packages and workspace members), and the names
//! and versions of its dependencies. It does not say what a manifest asked
//! for: no requirement string, no dependency kind (normal, dev, build), no
//! optional flag, no features, no target. Those live in `Cargo.toml`, so the
//! `REQUIRES` edges here carry only `origin: 'cargo.lock'` and say that the
//! resolved graph has the edge, not how the manifest declared it.
//!
//! # Minting
//!
//! Keys are purls (plan §2): `pkg:cargo/<name>` for a `Package`, and
//! `pkg:cargo/<name>@<version>` for a `PackageVersion`. Cargo versions may
//! carry build metadata (`1.1.8+spec-1.1.0`), and `+` is percent-encoded in
//! a purl version. Cargo names are used as written.
//!
//! # Anchoring and idempotence
//!
//! Every node carries `repository_id`, and every MERGE and MATCH names it,
//! so a fork ingesting the same lockfile never cross-links (the anchored
//! lookup rule of git_history/writer.rs). Re-ingesting an unchanged lockfile
//! matches every MERGE and rewrites the values the nodes already carry, so
//! the store's state does not change. A re-lock adds new versions as new
//! nodes and never duplicates a `Package`. The `DESCRIBES` edges from the
//! lockfile's `File` are replaced on every ingest, and so are the `REQUIRES`
//! edges of the versions it describes, because a workspace member keeps its
//! version while its dependencies change.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use cargo_lock::{Lockfile, SourceId};

use crate::store_graph::{self, GraphStore};
use crate::{Error, Result};
use serde_json::{Value, json};

/// The lockfile's path relative to the project root, and the `File` node
/// that DESCRIBES the versions it names.
pub const FILE: &str = "Cargo.lock";

/// The provenance tag on every edge this module writes (plan §2's `origin`).
pub const ORIGIN: &str = "cargo.lock";

/// The ecosystem segment of every purl this module mints.
const SYSTEM: &str = "cargo";

/// Rows per transaction (git_history/writer.rs's `BATCH`).
const BATCH: usize = 1000;

/// Where a locked package came from, as the lockfile names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// `registry` (crates.io or an alternative registry), `git`, `path` (no
    /// source line: a path dependency or a workspace member) or `other`
    /// (a local or directory registry).
    pub kind: &'static str,
    /// The source string as the lockfile writes it, when there is one.
    pub text: Option<String>,
    /// The registry or repository URL, for a git or registry source.
    pub url: Option<String>,
    /// The exact git revision the lockfile pins (git sources only).
    pub rev: Option<String>,
}

/// One locked package, after the duplicate-key rule of [`parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locked {
    pub name: String,
    pub version: String,
    pub source: Source,
    /// The `purl_at_version` of each dependency, deduplicated and sorted.
    pub requires: Vec<String>,
}

impl Locked {
    /// This version's `purl_at_version`.
    pub fn key(&self) -> String {
        purl_at_version(&self.name, &self.version)
    }
}

/// A parsed lockfile: one [`Locked`] per `purl_at_version`, in key order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    pub packages: Vec<Locked>,
    /// Entries that repeat an earlier `name@version` with another source
    /// (possible when a git and a registry copy share a version). The purl
    /// cannot tell them apart, so the first one in lockfile order is kept.
    pub shadowed: usize,
}

/// `pkg:cargo/<name>`.
pub fn purl(name: &str) -> String {
    format!("pkg:{SYSTEM}/{name}")
}

/// `pkg:cargo/<name>@<version>`, with the version percent-encoded the purl
/// way (`+` becomes `%2B`).
pub fn purl_at_version(name: &str, version: &str) -> String {
    let mut encoded = String::with_capacity(version.len());
    for byte in version.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("{}@{encoded}", purl(name))
}

/// Parse lockfile text. A malformed lockfile is an error; nothing is
/// written for it.
pub fn parse(text: &str) -> Result<Lock> {
    let lockfile: Lockfile = text
        .parse()
        .map_err(|error| Error::bad(format!("{FILE} is not a readable lockfile: {error}")))?;
    let mut by_key: BTreeMap<String, Locked> = BTreeMap::new();
    let mut shadowed = 0;
    for package in &lockfile.packages {
        let name = package.name.as_str().to_string();
        let version = package.version.to_string();
        let key = purl_at_version(&name, &version);
        if by_key.contains_key(&key) {
            shadowed += 1;
            continue;
        }
        let requires: BTreeSet<String> = package
            .dependencies
            .iter()
            .map(|dep| purl_at_version(dep.name.as_str(), &dep.version.to_string()))
            .collect();
        by_key.insert(
            key,
            Locked {
                name,
                version,
                source: source_of(package.source.as_ref()),
                requires: requires.into_iter().collect(),
            },
        );
    }
    Ok(Lock {
        packages: by_key.into_values().collect(),
        shadowed,
    })
}

fn source_of(source: Option<&SourceId>) -> Source {
    let Some(source) = source else {
        return Source {
            kind: "path",
            text: None,
            url: None,
            rev: None,
        };
    };
    let kind = if source.is_git() {
        "git"
    } else if source.is_registry() {
        "registry"
    } else {
        "other"
    };
    Source {
        kind,
        text: Some(source.to_string()),
        url: Some(source.url().to_string()),
        // `precise` is the git revision for a git source and cargo-lock's
        // `locked` marker for a registry one, so only git keeps it.
        rev: source
            .is_git()
            .then(|| source.precise().map(str::to_string))
            .flatten(),
    }
}

/// Read `<root>/Cargo.lock` and write its package facts into
/// `repository_id`. `None` when the project has no lockfile. The result is
/// the sync's `cargo_lock` entry: the counts written and the shadowed
/// entries skipped.
pub fn ingest(graph: &GraphStore, repository_id: &str, root: &Path) -> Result<Option<Value>> {
    let path = root.join(FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let lock = parse(&text)?;
    write(graph, repository_id, &lock).map(Some)
}

/// Write one parsed lockfile. Batches are per [`BATCH`] rows, one
/// transaction each, so a run killed midway leaves whole batches and the
/// next ingest converges.
pub fn write(graph: &GraphStore, repository_id: &str, lock: &Lock) -> Result<Value> {
    // The previous ingest of this file described some versions and carried
    // their REQUIRES from it. Both are dropped first (REQUIRES before the
    // DESCRIBES that find them), and the rows below write the current ones.
    graph.transaction(|db| {
        store_graph::write(
            db,
            "MERGE (f:File {repository_id: $repo, path: $path})",
            store_graph::params(&json!({"repo": repository_id, "path": FILE})),
        )?;
        store_graph::write(
            db,
            "MATCH (f:File {repository_id: $repo, path: $path})-[:DESCRIBES]->\
             (v:PackageVersion)-[r:REQUIRES {origin: 'cargo.lock'}]->() DELETE r",
            store_graph::params(&json!({"repo": repository_id, "path": FILE})),
        )?;
        store_graph::write(
            db,
            "MATCH (f:File {repository_id: $repo, path: $path})-[d:DESCRIBES]->(:PackageVersion) \
             DELETE d",
            store_graph::params(&json!({"repo": repository_id, "path": FILE})),
        )?;
        Ok(())
    })?;

    let mut packages = BTreeMap::new();
    let mut versions = Vec::new();
    let mut requires = Vec::new();
    for locked in &lock.packages {
        let key = locked.key();
        packages
            .entry(purl(&locked.name))
            .or_insert_with(|| json!({"purl": purl(&locked.name), "name": locked.name}));
        versions.push(json!({
            "key": key, "purl": purl(&locked.name), "name": locked.name,
            "version": locked.version, "source_kind": locked.source.kind,
            "source": locked.source.text, "source_url": locked.source.url,
            "source_rev": locked.source.rev,
        }));
        for target in &locked.requires {
            requires.push(json!({"from": key, "to": target}));
        }
    }
    let packages: Vec<Value> = packages.into_values().collect();
    let edges: Vec<Value> = lock
        .packages
        .iter()
        .map(|locked| {
            json!({"key": locked.key(), "purl": purl(&locked.name),
                   "version": locked.version})
        })
        .collect();
    let described: Vec<Value> = lock
        .packages
        .iter()
        .map(|locked| json!({"key": locked.key(), "path": FILE}))
        .collect();

    batches(
        graph,
        repository_id,
        &packages,
        "UNWIND $rows AS row \
         MERGE (pk:Package {repository_id: $repo, purl: row.purl}) \
         SET pk.system = 'cargo', pk.name = row.name",
    )?;
    batches(
        graph,
        repository_id,
        &versions,
        "UNWIND $rows AS row \
         MERGE (pv:PackageVersion {repository_id: $repo, purl_at_version: row.key}) \
         SET pv.purl = row.purl, pv.name = row.name, pv.version = row.version, \
             pv.source_kind = row.source_kind, pv.source = row.source, \
             pv.source_url = row.source_url, pv.source_rev = row.source_rev",
    )?;
    batches(
        graph,
        repository_id,
        &edges,
        "UNWIND $rows AS row \
         MATCH (pk:Package {repository_id: $repo, purl: row.purl}) \
         MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: row.key}) \
         MERGE (pk)-[:VERSION {version: row.version, origin: 'cargo.lock'}]->(pv)",
    )?;
    batches(
        graph,
        repository_id,
        &requires,
        "UNWIND $rows AS row \
         MATCH (a:PackageVersion {repository_id: $repo, purl_at_version: row.from}) \
         MATCH (b:PackageVersion {repository_id: $repo, purl_at_version: row.to}) \
         MERGE (a)-[:REQUIRES {origin: 'cargo.lock'}]->(b)",
    )?;
    batches(
        graph,
        repository_id,
        &described,
        "UNWIND $rows AS row \
         MATCH (f:File {repository_id: $repo, path: row.path}) \
         MATCH (pv:PackageVersion {repository_id: $repo, purl_at_version: row.key}) \
         MERGE (f)-[:DESCRIBES]->(pv)",
    )?;

    Ok(json!({
        "file": FILE,
        "packages": packages.len(),
        "versions": versions.len(),
        "requires": requires.len(),
        "described": described.len(),
        "shadowed": lock.shadowed,
    }))
}

/// Run `query` once per [`BATCH`]-row chunk of `rows`, one transaction per
/// chunk.
fn batches(graph: &GraphStore, repository_id: &str, rows: &[Value], query: &str) -> Result<()> {
    for chunk in rows.chunks(BATCH) {
        graph.transaction(|db| {
            store_graph::write(
                db,
                query,
                store_graph::params(&json!({"repo": repository_id, "rows": chunk})),
            )?;
            Ok(())
        })?;
    }
    Ok(())
}
