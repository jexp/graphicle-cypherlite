//! The knowledge-map family (software-visualization plan §5, D7): each
//! file's author knowledge joined to its churn, the 2x2 quadrant that
//! classifies it, and each folder's bus factor.
//!
//! # The join
//!
//! Knowledge is read through the history pack's ownership statements
//! ([`queries::file_authors`]: modifications of kind `M` per file and
//! author) and churn through its file churn ([`queries::file_churn`]:
//! changes per file, every kind). A file is scored when it has at least one
//! modification; an addition-only file has no author to read and is not in
//! the join.
//!
//! # Knowledge: the file's bus factor
//!
//! A file's knowledge is the bus factor: the fewest authors, most touches
//! first (ties to the smaller email), whose touches together exceed
//! [`SOLE_OWNER`] of the file's touches. It is one exactly when the history
//! ownership family's `sole_owner` flag holds, so the two packs agree on
//! every file. A sole owner is thin knowledge: the file's knowledge rests on
//! one person.
//!
//! # Quadrants: a median split of the scored files
//!
//! The thresholds come from the data, not a constant: `churn_median` is the
//! median churn of the scored files, `bus_factor_median` the median bus
//! factor. A file is high-churn when its churn is at or above
//! `churn_median`, and low-knowledge when its bus factor is at or below
//! `bus_factor_median`. The quadrants:
//!
//! | churn \ knowledge | low knowledge | high knowledge |
//! |-------------------|---------------|----------------|
//! | high churn        | `loss-risk`   | `shared-hot`   |
//! | low churn         | `dormant`     | `stable`       |
//!
//! # Folders and evidence
//!
//! A folder is a file's direct parent directory (`.` for the root). Its bus
//! factor is the same rule over the folder's pooled touches per author. Every
//! row is cited by its path: a file's path, a folder's path.
//!
//! The `window` parameter is echoed but not read: the join reads the whole
//! history, as the history pack's ownership family does. The `overlay`
//! parameter accepts `knowledge` (each file row adds its per-author
//! breakdown) and refuses `risk`, which is the risk-overlay family's column.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

use super::{Overlay, Params, View};
use crate::git_history::queries::{self, Author, SOLE_OWNER};
use crate::store_graph::GraphStore;
use crate::{Error, Result};

/// The rule the model names, so a reader sees the thresholds' meaning.
const RULE: &str = "knowledge = bus factor (fewest authors covering more than 50% of a file's \
    modifications); churn = changes per file; high churn at or above the median churn of the \
    scored files; low knowledge at or below the median bus factor; loss-risk = high churn and \
    low knowledge";

/// The section titles of the text report.
const FILES_TITLE: &str = "files by quadrant";
const FOLDERS_TITLE: &str = "folders by bus factor";

/// One quadrant of the churn-by-knowledge plane. The declaration order is the
/// priority order the file list is ranked by: the loss-risk files first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Quadrant {
    LossRisk,
    SharedHot,
    Stable,
    Dormant,
}

impl Quadrant {
    fn of(high_churn: bool, low_knowledge: bool) -> Self {
        match (high_churn, low_knowledge) {
            (true, true) => Self::LossRisk,
            (true, false) => Self::SharedHot,
            (false, false) => Self::Stable,
            (false, true) => Self::Dormant,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::LossRisk => "loss-risk",
            Self::SharedHot => "shared-hot",
            Self::Stable => "stable",
            Self::Dormant => "dormant",
        }
    }
}

/// A scored file: its churn, its authors, and what the join derives from them.
struct Scored {
    file: String,
    churn: i64,
    total: i64,
    bus_factor: usize,
    authors: Vec<Author>,
}

/// The knowledge-map view of `(repository, params)`.
pub fn view(graph: &GraphStore, repository: &str, params: &Params) -> Result<View> {
    if params.overlay == Overlay::Risk {
        return Err(Error::bad(
            "knowledge-map has no risk overlay; the risk-overlay family serves advisory exposure",
        ));
    }
    let churn = queries::file_churn(graph, repository)?;
    let files = scored(graph, repository, &churn)?;
    let churn_median = median(files.iter().map(|file| file.churn as f64).collect());
    let bus_factor_median = median(files.iter().map(|file| file.bus_factor as f64).collect());

    let mut quadrants: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut ranked: Vec<(Quadrant, &Scored)> = Vec::new();
    if let (Some(churn_cut), Some(knowledge_cut)) = (churn_median, bus_factor_median) {
        for file in &files {
            let quadrant = Quadrant::of(
                file.churn as f64 >= churn_cut,
                file.bus_factor as f64 <= knowledge_cut,
            );
            *quadrants.entry(quadrant.name()).or_default() += 1;
            ranked.push((quadrant, file));
        }
    }
    ranked.sort_by(|(qa, a), (qb, b)| {
        qa.cmp(qb)
            .then(b.churn.cmp(&a.churn))
            .then(a.file.cmp(&b.file))
    });
    let file_rows: Vec<Value> = ranked
        .iter()
        .take(params.cap)
        .map(|(quadrant, file)| file_row(*quadrant, file, params.overlay))
        .collect();

    let folder_rows = folder_rows(&files, params.cap);
    let model = json!({
        "rule": RULE,
        "thresholds": {"churn_median": churn_median, "bus_factor_median": bus_factor_median},
        "quadrants": quadrants,
        "files": file_rows,
        "folders": folder_rows,
    });
    Ok(View {
        sections: vec![(FILES_TITLE, file_rows), (FOLDERS_TITLE, folder_rows)],
        model,
        mermaid: None,
    })
}

/// The scored files, by path: every file with at least one modification.
fn scored(
    graph: &GraphStore,
    repository: &str,
    churn: &HashMap<String, (String, i64)>,
) -> Result<Vec<Scored>> {
    Ok(queries::file_authors(graph, repository)?
        .into_iter()
        .map(|(file, authors)| Scored {
            churn: churn.get(&file).map_or(0, |(_, changes)| *changes),
            total: authors.iter().map(|author| author.touches).sum(),
            bus_factor: bus_factor(&authors),
            file,
            authors,
        })
        .collect())
}

/// The bus factor of a set of authors (see the module docs): the fewest
/// authors, most touches first, whose touches exceed [`SOLE_OWNER`] of the
/// total. Zero only for an author-less set.
fn bus_factor(authors: &[Author]) -> usize {
    let mut ranked = authors.to_vec();
    ranked.sort_by(|a, b| b.touches.cmp(&a.touches).then(a.email.cmp(&b.email)));
    let total: i64 = ranked.iter().map(|author| author.touches).sum();
    let mut covered = 0;
    for (index, author) in ranked.iter().enumerate() {
        covered += author.touches;
        if covered as f64 / total as f64 > SOLE_OWNER {
            return index + 1;
        }
    }
    ranked.len()
}

/// The median of a list, the mean of the two middle values for an even count;
/// `None` for an empty list.
fn median(mut values: Vec<f64>) -> Option<f64> {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    match values.len() {
        0 => None,
        n if n % 2 == 1 => Some(values[mid]),
        _ => Some((values[mid - 1] + values[mid]) / 2.0),
    }
}

/// One file's row: its quadrant, churn and bus factor, its dominant author as
/// the ownership family names it, and the per-author breakdown under
/// `overlay=knowledge`.
fn file_row(quadrant: Quadrant, file: &Scored, overlay: Overlay) -> Value {
    let owner = queries::dominant(&file.authors);
    let mut row = json!({
        "evidence": file.file,
        "quadrant": quadrant.name(),
        "churn": file.churn,
        "bus_factor": file.bus_factor,
        "owner": owner.map(|author| author.name.as_str()),
        "owner_email": owner.map(|author| author.email.as_str()),
        "ownership": owner.map_or(0.0, |author| author.touches as f64 / file.total as f64),
        "total": file.total,
        "contributors": file.authors.len(),
    });
    if overlay == Overlay::Knowledge {
        let mut authors = file.authors.clone();
        authors.sort_by(|a, b| b.touches.cmp(&a.touches).then(a.email.cmp(&b.email)));
        row["authors"] = json!(
            authors
                .iter()
                .map(|author| json!({"email": author.email, "name": author.name,
                    "touches": author.touches}))
                .collect::<Vec<_>>()
        );
    }
    row
}

/// The folder rows, lowest bus factor first (the thinnest knowledge), then
/// the most touches, then the path; cut to `cap`. A folder is each file's
/// direct parent directory, its authors pooled over the files in it.
fn folder_rows(files: &[Scored], cap: usize) -> Vec<Value> {
    let mut folders: BTreeMap<String, Vec<&Scored>> = BTreeMap::new();
    for file in files {
        let folder = file.file.rsplit_once('/').map_or(".", |(parent, _)| parent);
        folders.entry(folder.to_string()).or_default().push(file);
    }
    let mut rows: Vec<(usize, i64, Value)> = folders
        .into_iter()
        .map(|(folder, members)| {
            let mut pooled: BTreeMap<String, Author> = BTreeMap::new();
            for author in members.iter().flat_map(|file| file.authors.iter()) {
                pooled
                    .entry(author.email.clone())
                    .and_modify(|sum| sum.touches += author.touches)
                    .or_insert_with(|| author.clone());
            }
            let pooled: Vec<Author> = pooled.into_values().collect();
            let total: i64 = pooled.iter().map(|author| author.touches).sum();
            let bus_factor = bus_factor(&pooled);
            let owner = queries::dominant(&pooled);
            let row = json!({
                "evidence": folder,
                "bus_factor": bus_factor,
                "files": members.len(),
                "owner": owner.map(|author| author.name.as_str()),
                "owner_email": owner.map(|author| author.email.as_str()),
                "ownership": owner.map_or(0.0, |author| author.touches as f64 / total as f64),
                "total": total,
                "contributors": pooled.len(),
            });
            (bus_factor, total, row)
        })
        .collect();
    rows.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(b.1.cmp(&a.1))
            .then(a.2["evidence"].as_str().cmp(&b.2["evidence"].as_str()))
    });
    rows.into_iter().take(cap).map(|(_, _, row)| row).collect()
}
