//! The coupling matrix (software-visualization plan §5, D3): folder by folder
//! co-change beside the static dependencies between the same folders.
//!
//! A cell is an unordered folder pair `a <= b`; a diagonal cell is a folder
//! with itself, listed once. Its co-change is the stored `CO_CHANGED` counts
//! of every file pair across the two folders, read in both windows (`all`
//! and `90d`). Its static reading is the number of file pairs the live
//! `CALLS`, `IMPORTS` and `IMPORTS_FROM` relations join across them; a file's
//! relations to itself do not count.
//!
//! Each cell is trend-colored from the 90d window against all of history,
//! and flagged where the two matrices disagree: hidden coupling (co-change
//! at or above [`HOT_CO`] with no static edge) and stale coupling (a static
//! edge with co-change below [`HOT_CO`]). Each cell cites its strongest file
//! pair as `evidence`. A focus (a folder or a file) narrows the matrix to the
//! focus folder's row and draws that row as mermaid.
//!
//! Every statement anchors on `repository_id` and never passes through
//! `(:Project)` (the supernode rule).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::{Params, View, Window, folder_of, int, mermaid_label, run, text};
use crate::Result;
use crate::git_history::queries::{STRUCTURAL, Section};
use crate::store_graph::GraphStore;

/// The co-change count at which a cell is hot: the history pack's reporting
/// threshold (`co-change` keeps pairs of more than two).
pub const HOT_CO: i64 = 3;

/// Every stored file co-change pair of one window, undirected, each once: the
/// coupling job keeps the smaller endpoint as the source, so `a < b` picks
/// the one row a pair has.
const CO_CHANGED: &str = "MATCH (a:File {repository_id: $repo})-[r:CO_CHANGED {window: $window}]-\
     (b:File {repository_id: $repo}) \
     WHERE a.path < b.path \
     RETURN a.path AS a, b.path AS b, r.count AS n";

/// One folder pair of the matrix, `a <= b`.
#[derive(Default)]
struct Cell {
    a: String,
    b: String,
    /// Co-change counts of the `all` window.
    co_all: i64,
    /// Co-change counts of the `90d` window.
    co_recent: i64,
    /// File pairs joined by a live static relation.
    dependencies: i64,
    /// The strongest file pair across the cell: `(strength, "x + y")`, where
    /// a co-change pair's strength is its count and a static pair's is zero.
    cited: Option<(i64, String)>,
}

impl Cell {
    /// The co-change count the window reads.
    fn co(&self, window: Window) -> i64 {
        match window {
            Window::All => self.co_all,
            Window::NinetyDays => self.co_recent,
        }
    }

    /// The row's evidence: the cited file pair.
    fn evidence(&self) -> &str {
        self.cited.as_ref().map_or("", |(_, pair)| pair)
    }

    /// Keeps the strongest citation; a tie goes to the smaller pair.
    fn cite(&mut self, strength: i64, pair: String) {
        let better = match &self.cited {
            None => true,
            Some((best, cited)) => strength > *best || (strength == *best && pair < *cited),
        };
        if better {
            self.cited = Some((strength, pair));
        }
    }

    /// The 90d reading against all of history: `rising` when at least half
    /// the history's co-change falls in the last ninety days, `cooling` when
    /// none does, `steady` between, and `absent` for a static-only cell.
    fn trend(&self) -> &'static str {
        if self.co_all == 0 && self.co_recent == 0 {
            "absent"
        } else if self.co_recent == 0 {
            "cooling"
        } else if self.co_recent * 2 >= self.co_all {
            "rising"
        } else {
            "steady"
        }
    }

    /// The disagreement between the two matrices, read in `window`.
    fn flag(&self, window: Window) -> &'static str {
        let co = self.co(window);
        if co >= HOT_CO && self.dependencies == 0 {
            "hidden"
        } else if self.dependencies > 0 && co < HOT_CO {
            "stale"
        } else {
            "none"
        }
    }
}

/// The focus of a call: what it named and the folder it resolved to. A focus
/// naming nothing in the repository resolves to no folder and scopes the
/// matrix to nothing.
struct Focus {
    input: String,
    folder: Option<String>,
}

/// The coupling matrix view of `repository` under `params`.
pub fn view(graph: &GraphStore, repository: &str, params: &Params) -> Result<View> {
    let (mut cells, files) = load(graph, repository)?;
    rank(&mut cells, params.window);
    let focus = params.focus.as_deref().map(|input| focus_of(input, &files));
    let scoped = scope(&cells, focus.as_ref());
    let window = params.window;
    let rows =
        |cells: &[&Cell]| -> Vec<Value> { cells.iter().map(|cell| row(cell, window)).collect() };

    let shown: Vec<&Cell> = scoped.iter().copied().take(params.cap).collect();
    let sections: Vec<Section> = vec![
        ("folder pairs", rows(&shown)),
        (
            "hidden coupling",
            rows(&flagged(&scoped, "hidden", window, params.cap)),
        ),
        (
            "stale coupling",
            rows(&flagged(&scoped, "stale", window, params.cap)),
        ),
    ];

    let mut folders: BTreeSet<String> = BTreeSet::new();
    for cell in &shown {
        folders.insert(cell.a.clone());
        folders.insert(cell.b.clone());
    }
    let focus_json = match &focus {
        None => Value::Null,
        Some(focus) => {
            if let Some(folder) = &focus.folder {
                folders.insert(folder.clone());
            }
            json!({"input": focus.input, "folder": focus.folder,
                "resolved": focus.folder.is_some()})
        }
    };
    let model = json!({"focus": focus_json, "window": window.key(),
        "folders": folders, "cells": rows(&shown)});
    let mermaid = match &focus {
        Some(Focus {
            input,
            folder: Some(folder),
        }) if !shown.is_empty() => Some(diagram(input, folder, &shown, window)),
        _ => None,
    };
    Ok(View {
        sections,
        model,
        mermaid,
    })
}

/// Every folder pair with a co-change pair or a static relation across it,
/// and the files of the repository the matrix reads.
fn load(graph: &GraphStore, repository: &str) -> Result<(Vec<Cell>, BTreeSet<String>)> {
    let mut cells: BTreeMap<(String, String), Cell> = BTreeMap::new();
    let mut files = BTreeSet::new();
    for window in [Window::All, Window::NinetyDays] {
        let args = json!({"repo": repository, "window": window.key()});
        for row in run(graph, CO_CHANGED, &args)? {
            let (a, b, count) = (text(&row, "a"), text(&row, "b"), int(&row, "n"));
            let cell = cell_of(&mut cells, &a, &b);
            match window {
                Window::All => cell.co_all += count,
                Window::NinetyDays => cell.co_recent += count,
            }
            cell.cite(count, pair(&a, &b));
            files.insert(a);
            files.insert(b);
        }
    }
    // Live relations, the file pairs they join: a pair of relations between
    // the same two files is one dependency.
    let mut linked = BTreeSet::new();
    for row in run(graph, STRUCTURAL, &json!({"repo": repository}))? {
        let (from, to) = (text(&row, "from"), text(&row, "to"));
        if !from.is_empty() && !to.is_empty() && from != to {
            linked.insert(ordered(from, to));
        }
    }
    for (a, b) in linked {
        let cell = cell_of(&mut cells, &a, &b);
        cell.dependencies += 1;
        cell.cite(0, pair(&a, &b));
        files.insert(a);
        files.insert(b);
    }
    Ok((cells.into_values().collect(), files))
}

/// The cell of two files' folders, created on first sight.
fn cell_of<'a>(cells: &'a mut BTreeMap<(String, String), Cell>, a: &str, b: &str) -> &'a mut Cell {
    let (fa, fb) = ordered(folder_of(a), folder_of(b));
    cells
        .entry((fa.clone(), fb.clone()))
        .or_insert_with(|| Cell {
            a: fa,
            b: fb,
            ..Cell::default()
        })
}

/// The focus of a call: a file the repository holds scopes to its folder, a
/// folder it holds scopes to itself, anything else to nothing.
fn focus_of(input: &str, files: &BTreeSet<String>) -> Focus {
    let folder = if files.contains(input) {
        Some(folder_of(input))
    } else if files.iter().any(|file| folder_of(file) == input) {
        Some(input.to_owned())
    } else {
        None
    };
    Focus {
        input: input.to_owned(),
        folder,
    }
}

/// The cells the matrix shows: all of them, or the focus folder's row.
fn scope<'a>(cells: &'a [Cell], focus: Option<&Focus>) -> Vec<&'a Cell> {
    match focus {
        None => cells.iter().collect(),
        Some(Focus {
            folder: Some(folder),
            ..
        }) => cells
            .iter()
            .filter(|cell| cell.a == *folder || cell.b == *folder)
            .collect(),
        Some(Focus { folder: None, .. }) => Vec::new(),
    }
}

/// The cells of one flag, cut to `cap`.
fn flagged<'a>(cells: &[&'a Cell], flag: &str, window: Window, cap: usize) -> Vec<&'a Cell> {
    cells
        .iter()
        .copied()
        .filter(|cell| cell.flag(window) == flag)
        .take(cap)
        .collect()
}

/// Canonical order: the window's co-change count, then the static count,
/// then the smaller citation, then the folder pair.
fn rank(cells: &mut [Cell], window: Window) {
    cells.sort_by(|x, y| {
        y.co(window)
            .cmp(&x.co(window))
            .then(y.dependencies.cmp(&x.dependencies))
            .then_with(|| x.evidence().cmp(y.evidence()))
            .then_with(|| (&x.a, &x.b).cmp(&(&y.a, &y.b)))
    });
}

/// One row: the citation first, then the cell's readings by name.
fn row(cell: &Cell, window: Window) -> Value {
    json!({"evidence": cell.evidence(), "a": cell.a, "b": cell.b,
        "co_all": cell.co_all, "co_90d": cell.co_recent,
        "static": cell.dependencies, "trend": cell.trend(),
        "flag": cell.flag(window)})
}

/// The focus row as mermaid: the focus folder first, its neighbors after in
/// folder order. Every label is quoted, and every node and edge carries its
/// citation or its readings.
fn diagram(input: &str, focus: &str, cells: &[&Cell], window: Window) -> String {
    let other = |cell: &Cell| -> String {
        if cell.a == focus {
            cell.b.clone()
        } else {
            cell.a.clone()
        }
    };
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    ids.insert(focus.to_owned(), "N0".to_owned());
    let mut neighbors: BTreeSet<String> = BTreeSet::new();
    for cell in cells {
        let folder = other(cell);
        if folder != focus {
            neighbors.insert(folder);
        }
    }
    for (index, folder) in neighbors.into_iter().enumerate() {
        ids.insert(folder, format!("N{}", index + 1));
    }

    let mut lines = vec!["graph LR".to_owned()];
    let focus_label = format!("\"{}<br/>{}\"", mermaid_label(focus), mermaid_label(input));
    lines.push(format!("  N0[{focus_label}]:::focus"));
    let mut drawn = BTreeSet::new();
    for cell in cells {
        let folder = other(cell);
        if folder != focus && drawn.insert(folder.clone()) {
            let folder_label = format!(
                "\"{}<br/>{}\"",
                mermaid_label(&folder),
                mermaid_label(cell.evidence())
            );
            lines.push(format!(
                "  {}[{folder_label}]:::{}",
                ids[&folder],
                cell.trend()
            ));
        }
    }
    for cell in cells {
        let flag = match cell.flag(window) {
            "hidden" => " hidden coupling",
            "stale" => " stale coupling",
            _ => "",
        };
        let label = format!(
            "co all={} 90d={} static={} {}{flag}",
            cell.co_all,
            cell.co_recent,
            cell.dependencies,
            cell.trend()
        );
        let edge_label = format!("\"{}\"", mermaid_label(&label));
        lines.push(format!("  N0 ---|{edge_label}| {}", ids[&other(cell)]));
    }
    lines.push("  classDef focus stroke-width:3px".to_owned());
    lines.push("  classDef rising fill:#f4c7c3,stroke:#b03a2e".to_owned());
    lines.push("  classDef steady fill:#fce8b2,stroke:#b7950b".to_owned());
    lines.push("  classDef cooling fill:#cfe2f3,stroke:#1f618d".to_owned());
    lines.push("  classDef absent fill:#eeeeee,stroke:#888888".to_owned());
    lines.join("\n") + "\n"
}

/// The citation of a file pair: both paths, in the canonical order.
fn pair(a: &str, b: &str) -> String {
    format!("{a} + {b}")
}

fn ordered(a: String, b: String) -> (String, String) {
    if a <= b { (a, b) } else { (b, a) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_statement_anchors_on_the_repository_not_the_project() {
        for cypher in [CO_CHANGED, STRUCTURAL] {
            assert!(cypher.contains("repository_id: $repo"), "{cypher}");
            assert!(!cypher.contains("Project"), "{cypher}");
        }
    }

    #[test]
    fn a_folder_is_the_directory_of_its_file_and_the_root_is_dot() {
        assert_eq!(folder_of("src/viz/mod.rs"), "src/viz");
        assert_eq!(folder_of("Cargo.toml"), ".");
    }
}
