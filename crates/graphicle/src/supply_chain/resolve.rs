//! The seam of supply-chain plan §3: a Rust `use` resolves to the locked
//! package whose crate it names, `(:Symbol)-[:RESOLVES_TO]->(:Package)`.
//!
//! # The rule
//!
//! The Rust extractor records each `use` as an `imports_from` edge to the
//! imported name, carrying the path as written (`_rust_use_path`,
//! extractors/rust.rs). The path's first segment names the crate —
//! `use serde::json` and `use serde::{Deserialize, Serialize}` both name
//! `serde` — so the imported-name symbol resolves to the name-level
//! `:Package pkg:cargo/serde` the lockfile wrote. The importing file stays
//! one hop away on the import edge itself, which carries its `relpath` and
//! line. A segment written with underscores may name a hyphenated crate
//! (`cargo_lock` is the crate `cargo-lock`), so resolution tries the
//! segment and then its hyphenated form.
//!
//! # What is not negative evidence
//!
//! A use path rooted in this crate never resolves: `crate`, `self` and
//! `super` heads, a head that names a `mod` item beside the use
//! (`_rust_use_local`, the extractor's scope-mod rule — a 2018-edition
//! uniform path whose first segment is a local module, not a crate), and a
//! head that names a workspace member, read from the lockfile's
//! path-sourced entries. Rust's standard library (`std`, `core`, `alloc`,
//! `proc_macro`, `test`) is not a locked crate either. Only an import that
//! matches none of those and no locked package is negative evidence: an
//! `:UnresolvedPackage` node with the reason, the `UnresolvedCross`
//! discipline — nothing is dropped silently, and every considered import
//! lands in one of the result's counts.
//!
//! # Limits
//!
//! The store's import facts carry no `extern crate` aliases, so a `use` of
//! an aliased crate resolves only when the alias spells the crate's name;
//! otherwise it lands as unresolved with the reason, visible rather than
//! silent. A local module that is neither a workspace member nor declared
//! beside the use it prefixes cannot be told from a crate either, so it
//! lands as unresolved for the same reason.
//!
//! # Where it runs, anchoring, idempotence
//!
//! The lockfile's packages are written by a sync ([`lock::ingest`]), the
//! import facts by a projection — neither order of arrival is guaranteed,
//! so both run the resolution: the sync after the lock ingest, the
//! projection job after it persists (`jobs.rs`). It is a full recompute —
//! the previous `RESOLVES_TO` edges and `UnresolvedPackage` nodes are
//! dropped first, like `lock::write` drops the `DESCRIBES` edges — so
//! re-running it on unchanged facts writes the same state. Every `MATCH`
//! and `MERGE` names `repository_id` (the symbol's `stable_id` and the
//! package's `purl` are the other keys), never the project supernode, so a
//! fork resolving the same imports never cross-links.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::Result;
use crate::store_graph::{self, GraphStore};

use super::lock;

/// Provenance on every edge and node this module writes: the package side
/// of the match is the lockfile's, so it carries lock.rs's tag.
pub const ORIGIN: &str = lock::ORIGIN;

/// The use-path heads that name Rust's standard library, never a crate.
const STDLIB: &[&str] = &["std", "core", "alloc", "proc_macro", "test"];

/// Rows per transaction (lock.rs's `BATCH`).
const BATCH: usize = 1000;

/// One live Rust import edge as the store holds it.
struct Import {
    /// The imported name's `stable_id`: the symbol that resolves.
    symbol: String,
    /// The use path as written (`serde::{Deserialize}`'s binding edge
    /// carries `serde::Deserialize`).
    use_path: String,
    /// The extractor marked the path rooted in this crate.
    local: bool,
    relpath: String,
    line: Option<i64>,
}

/// What one import's first segment matched.
enum Matched {
    /// The locked package's purl.
    Package(String),
    /// This crate's own path or member: never resolves, never negative.
    Local,
    /// Rust's standard library: not a locked crate.
    Stdlib,
}

/// The live Rust import edges, and how many the prefilter caught whose
/// facts then did not parse (counted, never dropped silently).
struct Imports {
    rows: Vec<Import>,
    unreadable: usize,
}

/// Resolve `repository_id`'s Rust imports against its locked packages: the
/// sync's `cargo_resolve` entry, and the projection job's. `None` when the
/// project has no lockfile packages to resolve against (a non-cargo
/// project, or one whose `Cargo.lock` never synced) — nothing to report,
/// nothing to rewrite.
pub fn resolve(graph: &GraphStore, repository_id: &str) -> Result<Option<Value>> {
    let packages = locked_packages(graph, repository_id)?;
    if packages.is_empty() {
        return Ok(None);
    }
    let members = workspace_members(graph, repository_id)?;
    let imports = imports(graph, repository_id)?;
    let mut edges: BTreeSet<(String, String)> = BTreeSet::new();
    let mut unresolved: BTreeMap<(String, String), Value> = BTreeMap::new();
    let mut counts = Counts {
        unreadable: imports.unreadable,
        ..Counts::default()
    };
    for import in &imports.rows {
        let head = head_of(&import.use_path);
        match classify(import, &head, &members, &packages) {
            Some(Matched::Package(purl)) => {
                counts.resolved += 1;
                edges.insert((import.symbol.clone(), purl));
            }
            Some(Matched::Local) => counts.local += 1,
            Some(Matched::Stdlib) => counts.stdlib += 1,
            None => {
                counts.unresolved += 1;
                // One node per (symbol, crate): the first site's provenance
                // travels with it, the sites stay on their import edges.
                let mut row = json!({
                    "origin": ORIGIN, "symbol": import.symbol, "package": head,
                    "use_path": import.use_path, "relpath": import.relpath,
                    "reason": format!(
                        "no locked crate named '{head}' and no workspace member of that name"
                    ),
                });
                if let Some(line) = import.line {
                    row["line"] = json!(line);
                }
                unresolved
                    .entry((import.symbol.clone(), head.clone()))
                    .or_insert(row);
            }
        }
    }
    let nodes: Vec<Value> = unresolved.into_values().collect();
    write(graph, repository_id, &edges, &nodes)?;
    Ok(Some(json!({
        // Every considered import lands in exactly one of these five.
        "imports": imports.rows.len() + imports.unreadable,
        "resolved": counts.resolved,
        "local": counts.local,
        "stdlib": counts.stdlib,
        "unresolved": counts.unresolved,
        "unreadable": counts.unreadable,
        "unresolved_packages": nodes.len(),
        "edges": edges.len(),
    })))
}

/// The counts one run reports: the buckets every considered import lands
/// in, so none is dropped silently.
#[derive(Default)]
struct Counts {
    resolved: usize,
    local: usize,
    stdlib: usize,
    unresolved: usize,
    /// Import edges the prefilter caught whose stored facts then did not
    /// parse; rare (the projection writes canonical JSON), but counted.
    unreadable: usize,
}

/// The locked packages to resolve against: crate name → purl.
fn locked_packages(graph: &GraphStore, repository_id: &str) -> Result<BTreeMap<String, String>> {
    Ok(graph
        .read(|db| {
            store_graph::rows(
                db,
                "MATCH (pk:Package {repository_id: $repo, system: 'cargo'}) \
                 RETURN pk.name AS name, pk.purl AS purl",
                params(&json!({"repo": repository_id})),
            )
        })?
        .into_iter()
        .filter_map(|row| {
            Some((
                row["name"].as_str()?.to_string(),
                row["purl"].as_str()?.to_string(),
            ))
        })
        .collect())
}

/// The workspace's member names: the lockfile's path-sourced entries. A use
/// path headed by one of them is a local module (2018-edition uniform
/// paths), not an import.
fn workspace_members(graph: &GraphStore, repository_id: &str) -> Result<BTreeSet<String>> {
    Ok(graph
        .read(|db| {
            store_graph::rows(
                db,
                "MATCH (pv:PackageVersion {repository_id: $repo, source_kind: 'path'}) \
                 RETURN DISTINCT pv.name AS name",
                params(&json!({"repo": repository_id})),
            )
        })?
        .into_iter()
        .filter_map(|row| row["name"].as_str().map(str::to_string))
        .collect())
}

/// The live Rust import edges: `imports_from` edges whose facts carry a use
/// path. The `CONTAINS` is a prefilter only — the parse below decides, so a
/// path that merely spells the marker (`foo::_rust_use_path_marker`) is
/// read as the absence it is.
fn imports(graph: &GraphStore, repository_id: &str) -> Result<Imports> {
    let rows = graph.read(|db| {
        store_graph::rows(
            db,
            "MATCH (a:Symbol {repository_id: $repo})-[r:IMPORTS_FROM]->(b:Symbol {repository_id: $repo}) \
             WHERE r.valid_from_episode IS NOT NULL AND r.data CONTAINS '_rust_use_path' \
             RETURN b.stable_id AS symbol, r.data AS data, r.relpath AS relpath, \
                    r.source_location AS location \
             ORDER BY symbol, relpath, location",
            params(&json!({"repo": repository_id})),
        )
    })?;
    let mut out = Imports {
        rows: Vec::with_capacity(rows.len()),
        unreadable: 0,
    };
    for row in rows {
        let data: Value = match serde_json::from_str(row["data"].as_str().unwrap_or_default()) {
            Ok(data) => data,
            Err(_) => {
                // The projection writes canonical JSON, so this is
                // corruption; it is counted, never dropped silently.
                out.unreadable += 1;
                continue;
            }
        };
        // The prefilter's false positive: a path that spells the marker
        // (`foo::_rust_use_path_marker`) parses to no use-path key, and is
        // read as the absence it is.
        let Some(use_path) = data["_rust_use_path"]
            .as_str()
            .filter(|path| !path.is_empty())
        else {
            continue;
        };
        out.rows.push(Import {
            symbol: row["symbol"].as_str().unwrap_or_default().to_string(),
            use_path: use_path.to_string(),
            local: data["_rust_use_local"] == true,
            relpath: row["relpath"].as_str().unwrap_or_default().to_string(),
            line: line_of(row["location"].as_str().unwrap_or_default()),
        });
    }
    Ok(out)
}

/// What `import`'s head matched, or `None` when it matched nothing. Local
/// paths and members come first (a member named like a crate shadows it,
/// as a `mod` item does), then the locked packages, then the standard
/// library.
fn classify(
    import: &Import,
    head: &str,
    members: &BTreeSet<String>,
    packages: &BTreeMap<String, String>,
) -> Option<Matched> {
    if import.local
        || matches!(head, "crate" | "self" | "super")
        || (!head.is_empty() && members.contains(head))
    {
        return Some(Matched::Local);
    }
    // `cargo_lock` is the crate `cargo-lock`: a segment written with
    // underscores may name a hyphenated crate.
    let hyphenated = head.contains('_').then(|| head.replace('_', "-"));
    if let Some(purl) = packages
        .get(head)
        .or_else(|| hyphenated.as_deref().and_then(|name| packages.get(name)))
    {
        return Some(Matched::Package(purl.clone()));
    }
    if STDLIB.contains(&head) {
        return Some(Matched::Stdlib);
    }
    None
}

/// The first segment of a use path: the crate it names. A leading `::`
/// (the 2018 uniform-path crate form) is stripped.
fn head_of(use_path: &str) -> String {
    use_path
        .trim_start_matches(':')
        .split("::")
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// The line of a `source_location` (`L12`), when it holds one.
pub(crate) fn line_of(location: &str) -> Option<i64> {
    location.strip_prefix('L')?.parse().ok()
}

/// Rewrite the resolution: drop the previous edges and nodes, then write
/// the current ones, whole batches per transaction (lock.rs's `write`).
fn write(
    graph: &GraphStore,
    repository_id: &str,
    edges: &BTreeSet<(String, String)>,
    unresolved: &[Value],
) -> Result<()> {
    graph.transaction(|db| {
        store_graph::write(
            db,
            "MATCH (:Symbol {repository_id: $repo})-[r:RESOLVES_TO {origin: $origin}]->() DELETE r",
            params(&json!({"repo": repository_id, "origin": ORIGIN})),
        )?;
        store_graph::write(
            db,
            "MATCH (u:UnresolvedPackage {repository_id: $repo, origin: $origin}) DETACH DELETE u",
            params(&json!({"repo": repository_id, "origin": ORIGIN})),
        )?;
        Ok(())
    })?;
    let rows: Vec<Value> = edges
        .iter()
        .map(|(symbol, purl)| json!({"origin": ORIGIN, "symbol": symbol, "purl": purl}))
        .collect();
    batches(
        graph,
        repository_id,
        &rows,
        "UNWIND $rows AS row \
         MATCH (s:Symbol {repository_id: $repo, stable_id: row.symbol}) \
         MATCH (pk:Package {repository_id: $repo, purl: row.purl}) \
         MERGE (s)-[:RESOLVES_TO {origin: row.origin}]->(pk)",
    )?;
    batches(
        graph,
        repository_id,
        unresolved,
        "UNWIND $rows AS row \
         MERGE (u:UnresolvedPackage {repository_id: $repo, origin: row.origin, \
             symbol: row.symbol, package: row.package}) \
         SET u += row",
    )?;
    Ok(())
}

/// Run `query` once per [`BATCH`]-row chunk of `rows`, one transaction per
/// chunk (lock.rs's `batches`).
fn batches(graph: &GraphStore, repository_id: &str, rows: &[Value], query: &str) -> Result<()> {
    for chunk in rows.chunks(BATCH) {
        graph.transaction(|db| {
            store_graph::write(
                db,
                query,
                params(&json!({"repo": repository_id, "rows": chunk})),
            )?;
            Ok(())
        })?;
    }
    Ok(())
}

/// lock.rs's parameter helper: the same shape, one module over.
fn params(value: &Value) -> store_graph::Params {
    store_graph::params(value)
}
