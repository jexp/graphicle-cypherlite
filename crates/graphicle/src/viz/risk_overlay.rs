//! The `risk-overlay` family (software-visualization plan §5, D8): advisory
//! exposure of files and symbols, read off the join the supply-chain tools
//! walk — `(:Symbol)-[:RESOLVES_TO]->(:Package)-[:VERSION]->
//! (:PackageVersion)-[:AFFECTED_BY]->(:Advisory)`, and the file side through
//! the symbol's `IMPORTS_FROM` (its `relpath` and line). The query shapes are
//! `supply_chain::tools`' own (`sites`, `versions`, `ADVISORIES`, `SCANNED`,
//! `files_json`), so an advisory's files and advisories are `vuln_impact`'s
//! for the same advisory, by construction and by test.
//!
//! One view, two outputs over the same rows: the overlay column (a per-file
//! exposure value, which is the ranked file list's own projection) and the
//! standalone list of files by exposure, each with the advisories it reaches
//! and their `fixed_in`. Symbols and advisories are listed beside them. With
//! `focus` the view scopes to one file, symbol or package, and its exposure
//! subgraph is the mermaid diagram.
//!
//! Exposure is the number of distinct advisories a file's imports reach
//! through the locked versions of the packages they resolve to. Every ranked
//! list sorts by exposure (advisories reached; files, for an advisory), then
//! by the smaller evidence, then by id, and is cut to `cap`. A nested citation
//! list (an advisory's files) is complete, so it matches `vuln_impact`.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::{Params, View, mermaid_label, text};
use crate::Result;
use crate::store_graph::GraphStore;
use crate::supply_chain::tools::{self, Site};

/// The section titles of the text report.
const FILES: &str = "Advisory exposure by file";
const SYMBOLS: &str = "Advisory exposure by symbol";
const ADVISORY_ROWS: &str = "Advisories reached";

/// One advisory a locked version carries, as the version's record holds it.
#[derive(Clone, Debug, PartialEq)]
struct Hit {
    osv_id: String,
    title: String,
    url: String,
    purl_at_version: String,
    version: String,
    fixed_in: Value,
}

impl Hit {
    fn json(&self) -> Value {
        json!({"osv_id": self.osv_id, "title": self.title, "url": self.url,
            "purl_at_version": self.purl_at_version, "version": self.version,
            "fixed_in": self.fixed_in})
    }
}

/// One candidate row of a ranked list: its rank, the evidence and id the ties
/// break on, the section row it prints as, and the model entry it serves.
struct Ranked {
    rank: usize,
    evidence: String,
    id: String,
    row: Value,
    entry: Value,
}

/// The `risk-overlay` family's view of `(repository, params)`.
pub fn view(graph: &GraphStore, repository: &str, params: &Params) -> Result<View> {
    let focus = params.focus.as_deref();
    let sites = scope(graph, repository, focus)?;
    let mut hits: BTreeMap<String, Vec<Hit>> = BTreeMap::new();
    for site in &sites {
        if !hits.contains_key(&site.purl) {
            let found = hits_of(graph, repository, &site.purl)?;
            hits.insert(site.purl.clone(), found);
        }
    }
    let exposed: Vec<usize> = (0..sites.len())
        .filter(|&i| !hits_at(&hits, &sites[i]).is_empty())
        .collect();

    // Files: the exposed import sites grouped by file, in site order.
    let mut by_file: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for &i in &exposed {
        by_file.entry(sites[i].path.as_str()).or_default().push(i);
    }
    let files_found = by_file.len();
    let mut file_candidates = Vec::new();
    for (path, idx) in &by_file {
        let found = distinct(idx.iter().flat_map(|&i| hits_at(&hits, &sites[i])));
        let advisories = osv_ids(&found);
        let packages: BTreeSet<&str> = idx.iter().map(|&i| sites[i].purl.as_str()).collect();
        let cited: Vec<String> = idx.iter().map(|&i| sites[i].citation()).collect();
        let evidence = cited[0].clone();
        file_candidates.push(Ranked {
            rank: advisories.len(),
            evidence: evidence.clone(),
            id: (*path).to_owned(),
            row: json!({"evidence": evidence, "file": path, "exposure": advisories.len(),
                "advisories": join(advisories.iter().copied()),
                "packages": join(packages.iter().copied())}),
            entry: json!({"evidence": evidence, "path": path, "exposure": advisories.len(),
                "advisories": found.iter().map(|hit| hit.json()).collect::<Vec<_>>(),
                "packages": packages, "cited": cited}),
        });
    }

    // Symbols: the imported names, per package they resolve to.
    let mut by_symbol: BTreeMap<(&str, &str), Vec<usize>> = BTreeMap::new();
    for &i in &exposed {
        by_symbol
            .entry((sites[i].symbol.as_str(), sites[i].purl.as_str()))
            .or_default()
            .push(i);
    }
    let symbols_found = by_symbol.len();
    let mut symbol_candidates = Vec::new();
    for ((symbol, purl), idx) in &by_symbol {
        let found = distinct(idx.iter().flat_map(|&i| hits_at(&hits, &sites[i])));
        let advisories = osv_ids(&found);
        let cited: Vec<String> = idx.iter().map(|&i| sites[i].citation()).collect();
        let evidence = cited[0].clone();
        symbol_candidates.push(Ranked {
            rank: advisories.len(),
            evidence: evidence.clone(),
            id: format!("{symbol} {purl}"),
            row: json!({"evidence": evidence, "symbol": symbol, "package": purl,
                "exposure": advisories.len(), "advisories": join(advisories.iter().copied())}),
            entry: json!({"evidence": evidence, "symbol": symbol, "package": purl,
                "name": sites[idx[0]].name, "exposure": advisories.len(),
                "advisories": found.iter().map(|hit| hit.json()).collect::<Vec<_>>(),
                "cited": cited}),
        });
    }

    // Advisories: the sites whose packages carry each one, and the versions.
    let mut by_osv: BTreeMap<&str, Reach> = BTreeMap::new();
    for &i in &exposed {
        for hit in hits_at(&hits, &sites[i]) {
            let reach = by_osv.entry(hit.osv_id.as_str()).or_insert_with(|| Reach {
                title: hit.title.clone(),
                url: hit.url.clone(),
                sites: BTreeSet::new(),
                versions: BTreeMap::new(),
            });
            reach.sites.insert(i);
            reach.versions.insert(
                hit.purl_at_version.clone(),
                (hit.version.clone(), hit.fixed_in.clone()),
            );
        }
    }
    let advisories_found = by_osv.len();
    let mut advisory_candidates = Vec::new();
    for (osv, reach) in &by_osv {
        // Grouped by package, each package's sites in citation order: the
        // order `vuln_impact` lists an advisory's files in.
        let mut members: Vec<&Site> = reach.sites.iter().map(|&i| &sites[i]).collect();
        members.sort_by(|a, b| a.purl.cmp(&b.purl));
        let files: BTreeSet<&str> = members.iter().map(|site| site.path.as_str()).collect();
        let evidence = reach
            .sites
            .iter()
            .next()
            .map(|&i| sites[i].citation())
            .unwrap_or_default();
        let fixed: BTreeSet<&str> = reach
            .versions
            .values()
            .filter_map(|(_, fixed)| fixed.as_str())
            .collect();
        let fixed_text = if fixed.is_empty() {
            "none".to_owned()
        } else {
            join(fixed.iter().copied())
        };
        advisory_candidates.push(Ranked {
            rank: files.len(),
            evidence: evidence.clone(),
            id: (*osv).to_owned(),
            row: json!({"evidence": evidence, "advisory": osv, "files": files.len(),
                "fixed_in": fixed_text}),
            entry: json!({"evidence": evidence, "osv_id": osv, "title": reach.title,
                "url": reach.url, "exposure": files.len(),
                "versions": reach.versions.iter().map(|(key, (version, fixed_in))| json!({
                    "purl_at_version": key, "version": version, "fixed_in": fixed_in}))
                    .collect::<Vec<_>>(),
                "files": tools::files_json(&members), "citations": members.len()}),
        });
    }

    let files = top(file_candidates, params.cap);
    let symbols = top(symbol_candidates, params.cap);
    let advisories = top(advisory_candidates, params.cap);

    let shown: BTreeSet<String> = files
        .iter()
        .filter_map(|file| file.entry["path"].as_str().map(str::to_owned))
        .collect();
    let mermaid = match focus {
        Some(_) => diagram(&sites, &hits, &exposed, &shown),
        None => None,
    };

    let scanned = tools::rows(graph, tools::SCANNED, json!({"repo": repository}))?
        .first()
        .and_then(|row| row["n"].as_i64())
        .unwrap_or(0)
        > 0;
    let mut not_known = Vec::new();
    if !scanned {
        not_known.push("OSV has not scanned this project's lockfile yet".to_owned());
    }
    if let Some(focus) = focus {
        if sites.is_empty() {
            not_known.push(format!(
                "the code graph holds no import of {focus} that resolves to a locked package"
            ));
        } else if exposed.is_empty() {
            not_known.push(format!("no advisory reaches {focus}"));
        }
    }

    let column: Vec<Value> = files
        .iter()
        .map(|file| json!({"path": file.entry["path"], "value": file.entry["exposure"]}))
        .collect();
    let model = json!({
        "focus": focus,
        "overlay": {"column": "risk", "values": column},
        "files": files.iter().map(|file| file.entry.clone()).collect::<Vec<_>>(),
        "symbols": symbols.iter().map(|symbol| symbol.entry.clone()).collect::<Vec<_>>(),
        "advisories": advisories.iter().map(|advisory| advisory.entry.clone()).collect::<Vec<_>>(),
        "summary": {"files": files_found, "symbols": symbols_found,
            "advisories": advisories_found, "citations": exposed.len()},
        "not_known": not_known,
    });
    Ok(View {
        sections: vec![
            (FILES, files.iter().map(|file| file.row.clone()).collect()),
            (
                SYMBOLS,
                symbols.iter().map(|symbol| symbol.row.clone()).collect(),
            ),
            (
                ADVISORY_ROWS,
                advisories
                    .iter()
                    .map(|advisory| advisory.row.clone())
                    .collect(),
            ),
        ],
        model,
        mermaid,
    })
}

/// The import sites in scope: all of them, or for a `focus` the sites of the
/// file, symbol or package it names (the supply-chain tools' own filters).
fn scope(graph: &GraphStore, repository: &str, focus: Option<&str>) -> Result<Vec<Site>> {
    let Some(focus) = focus else {
        return tools::sites(graph, repository, None);
    };
    let mut found = tools::sites(graph, repository, Some((tools::FROM_FILE, focus)))?;
    found.extend(tools::sites(
        graph,
        repository,
        Some((tools::FROM_SYMBOL, focus)),
    )?);
    for purl in tools::packages_named(graph, repository, focus)? {
        found.extend(tools::sites(
            graph,
            repository,
            Some((tools::FROM_PACKAGE, &purl)),
        )?);
    }
    found.sort_by(|a, b| {
        (&a.path, a.line, &a.purl, &a.symbol).cmp(&(&b.path, b.line, &b.purl, &b.symbol))
    });
    found.dedup();
    Ok(found)
}

/// The advisories of every locked version of `purl` in `repository`: the
/// version's `ADVISORIES` rows, with the `fixed_in` each edge carries.
fn hits_of(graph: &GraphStore, repository: &str, purl: &str) -> Result<Vec<Hit>> {
    let mut hits = Vec::new();
    for facts in tools::versions(graph, repository, purl)? {
        for row in tools::rows(
            graph,
            tools::ADVISORIES,
            json!({"repo": repository, "key": facts.key}),
        )? {
            hits.push(Hit {
                osv_id: text(&row, "osv_id"),
                title: text(&row, "title"),
                url: text(&row, "url"),
                purl_at_version: facts.key.clone(),
                version: facts.version.clone(),
                fixed_in: row["fixed_in"].clone(),
            });
        }
    }
    hits.sort_by(|a, b| (&a.osv_id, &a.purl_at_version).cmp(&(&b.osv_id, &b.purl_at_version)));
    Ok(hits)
}

/// The hits of the package a site resolves to.
fn hits_at<'a>(hits: &'a BTreeMap<String, Vec<Hit>>, site: &Site) -> &'a [Hit] {
    hits.get(&site.purl).map_or(&[][..], Vec::as_slice)
}

/// The advisory's reach: its title and url, the import sites that resolve to
/// a package it affects (by index into the scope), and the versions it hits.
struct Reach {
    title: String,
    url: String,
    sites: BTreeSet<usize>,
    versions: BTreeMap<String, (String, Value)>,
}

/// The hits of a group, one per (advisory, version).
fn distinct<'a>(hits: impl Iterator<Item = &'a Hit>) -> Vec<&'a Hit> {
    let mut seen: BTreeMap<(&str, &str), &Hit> = BTreeMap::new();
    for hit in hits {
        seen.entry((hit.osv_id.as_str(), hit.purl_at_version.as_str()))
            .or_insert(hit);
    }
    seen.into_values().collect()
}

/// The distinct advisory ids of a group's hits.
fn osv_ids<'a>(hits: &[&'a Hit]) -> BTreeSet<&'a str> {
    hits.iter().map(|hit| hit.osv_id.as_str()).collect()
}

/// A list's rows in its canonical order — rank descending, then the smaller
/// evidence, then the smaller id — cut to `cap`.
fn top(mut ranked: Vec<Ranked>, cap: usize) -> Vec<Ranked> {
    ranked.sort_by(|a, b| {
        (Reverse(a.rank), &a.evidence, &a.id).cmp(&(Reverse(b.rank), &b.evidence, &b.id))
    });
    ranked.truncate(cap);
    ranked
}

/// The exposure subgraph of the files shown: file -> symbol -> package ->
/// locked version -> advisory, every node labeled with its citation or name.
/// `None` when no shown file is exposed.
fn diagram(
    sites: &[Site],
    hits: &BTreeMap<String, Vec<Hit>>,
    exposed: &[usize],
    shown: &BTreeSet<String>,
) -> Option<String> {
    let mut nodes: BTreeMap<(u8, String), String> = BTreeMap::new();
    let mut edges: BTreeSet<((u8, String), (u8, String))> = BTreeSet::new();
    for &i in exposed {
        let site = &sites[i];
        if !shown.contains(&site.path) {
            continue;
        }
        let file = node(&mut nodes, 0, site.path.clone(), site.citation());
        let symbol = node(
            &mut nodes,
            1,
            format!("{} {}", site.symbol, site.purl),
            site.symbol.clone(),
        );
        let package = node(&mut nodes, 2, site.purl.clone(), site.purl.clone());
        edges.insert((file, symbol.clone()));
        edges.insert((symbol, package.clone()));
        for hit in hits_at(hits, site) {
            let version = node(
                &mut nodes,
                3,
                hit.purl_at_version.clone(),
                hit.purl_at_version.clone(),
            );
            let advisory = node(&mut nodes, 4, hit.osv_id.clone(), hit.osv_id.clone());
            edges.insert((package.clone(), version.clone()));
            edges.insert((version, advisory));
        }
    }
    if edges.is_empty() {
        return None;
    }
    let ids: BTreeMap<(u8, String), String> = nodes
        .keys()
        .cloned()
        .enumerate()
        .map(|(n, key)| (key, format!("n{n}")))
        .collect();
    let mut lines = vec!["graph LR".to_owned()];
    for (key, label) in &nodes {
        lines.push(format!("  {}[\"{}\"]", ids[key], mermaid_label(label)));
    }
    for (from, to) in &edges {
        lines.push(format!("  {} --> {}", ids[from], ids[to]));
    }
    Some(lines.join("\n") + "\n")
}

/// Declare a diagram node once, under its kind and key, and return its key.
fn node(
    nodes: &mut BTreeMap<(u8, String), String>,
    kind: u8,
    key: String,
    label: String,
) -> (u8, String) {
    nodes.entry((kind, key.clone())).or_insert(label);
    (kind, key)
}

fn join<'a>(items: impl Iterator<Item = &'a str>) -> String {
    items.collect::<Vec<_>>().join(", ")
}
