//! The derived view files (ADR-0406 §2): `index.md` (a directory
//! listing, progressive disclosure) and `log.md` (a human-readable
//! changelog with OKF's ISO `YYYY-MM-DD` date headings). Reserved at
//! any hierarchy level, skipped on load, regenerated wholesale at every
//! checkpoint — views, never load-bearing state; external edits are
//! simply overwritten by the next regeneration.

use std::collections::{BTreeMap, BTreeSet};

use super::{checkpoint::NodePlan, VAULT_LABEL};
use cypherlite_core::temporal::civil_from_days;
use cypherlite_markdown::{INDEX_VIEW, LOG_VIEW};

/// One changelog entry relative to the previous checkpoint: a file
/// added, changed, or removed — one vocabulary, not three parallel
/// lists every producer and consumer re-spells. The declaration order
/// (added, changed, removed) is the changelog's grouping order: the
/// derived ordering is the rule, never a parallel rank table.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Change {
    Added(String),
    Changed(String),
    Removed(String),
}

/// Render every view file for the planned vault: the root `index.md`,
/// one `index.md` per label directory, and the root `log.md`. The
/// inputs are the plan's typed per-node decision (a view never
/// re-parses the serialized path encoding) and the checkpoint's wall
/// clock, read once by the orchestrator (the graph's commit counter is
/// not a clock).
pub(super) fn render(
    plans: &BTreeMap<i64, NodePlan>,
    changes: &[Change],
    checkpoint_millis: u64,
) -> Vec<(String, String)> {
    let mut labels: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // The planned vault keys — the changelog's linkability set.
    let mut keys: BTreeSet<&str> = BTreeSet::new();
    for plan in plans.values() {
        keys.insert(plan.vault_key.as_str());
        // Root files render no label index (the vault root has none).
        if plan.label_dir != VAULT_LABEL {
            labels
                .entry(plan.label_dir.clone())
                .or_default()
                .push(plan.vault_key.clone());
        }
    }
    let mut out = Vec::new();
    out.push((INDEX_VIEW.to_string(), root_index(&labels)));
    for (label, label_keys) in &labels {
        out.push((
            format!("{label}/{INDEX_VIEW}"),
            label_index(label, label_keys),
        ));
    }
    out.push((
        LOG_VIEW.to_string(),
        changelog(changes, &keys, checkpoint_millis),
    ));
    out
}

fn root_index(labels: &BTreeMap<String, Vec<String>>) -> String {
    let mut text = String::from("# Index\n\n");
    for (label, keys) in labels {
        text.push_str(&format!(
            "- **{label}** ({count}) — see [[{label}/index]]\n",
            count = keys.len()
        ));
    }
    text.push('\n');
    text
}

fn label_index(label: &str, keys: &[String]) -> String {
    let mut text = format!("# {label}\n\n");
    for key in keys {
        text.push_str(&format!("- [[{key}]]\n"));
    }
    text.push('\n');
    text
}

fn changelog(changes: &[Change], keys: &BTreeSet<&str>, checkpoint_millis: u64) -> String {
    let mut text = String::from("# Log\n\n");
    if changes.is_empty() {
        text.push_str("No changes in this checkpoint.\n");
        return text;
    }
    let date = iso_date(checkpoint_millis);
    text.push_str(&format!("## {date}\n"));
    // Grouped by kind in the enum's declaration order (the derived
    // ordering), paths ascending within each group.
    let mut entries = changes.to_vec();
    entries.sort();
    for entry in &entries {
        let (verb, display) = entry.display(keys);
        text.push_str(&format!("- **{verb}** {display}\n"));
    }
    text
}

impl Change {
    /// The changelog line's two halves: the verb and the display form —
    /// an added or changed file links its vault key, a removed file has
    /// nothing to link to and lists its path.
    fn display(&self, keys: &BTreeSet<&str>) -> (&'static str, String) {
        match self {
            Change::Added(path) => ("Added", view_link(path, keys)),
            Change::Changed(path) => ("Changed", view_link(path, keys)),
            Change::Removed(path) => ("Removed", path.to_string()),
        }
    }
}

/// An added or changed file's display: its vault key as a wikilink when
/// the key is planned (a label-directory file), else the bare path — a
/// root file's vault key is `Vault/name`, not the path minus `.md`, so
/// its path is its own display.
fn view_link(path: &str, keys: &BTreeSet<&str>) -> String {
    match path.strip_suffix(".md") {
        Some(key) if keys.contains(key) => format!("[[{key}]]"),
        _ => path.to_string(),
    }
}

/// Civil date from epoch millis, through the one inverse
/// (`cypherlite_core::temporal::civil_from_days`, the standard
/// days-to-y/m/d algorithm — deterministic, dependency-free).
fn iso_date(millis: u64) -> String {
    let (y, m, d) = civil_from_days(i128::from(millis / 86_400_000));
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_date_matches_known_dates() {
        assert_eq!(iso_date(0), "1970-01-01");
        // 2026-10-25 is 20_751 days after the epoch.
        assert_eq!(iso_date(20_751 * 86_400_000), "2026-10-25");
        assert_eq!(iso_date(1_136_073_600_000), "2006-01-01");
    }

    #[test]
    fn changelog_groups_under_the_checkpoint_date() {
        // Deliberately out of order: the derived grouping order
        // (added, changed, removed) is what the log shows.
        let changes = vec![
            Change::Removed("Person/Old.md".into()),
            Change::Added("Person/Ada.md".into()),
            Change::Changed("Person/Grace.md".into()),
        ];
        let keys = BTreeSet::from(["Person/Ada", "Person/Grace"]);
        let text = changelog(&changes, &keys, 0);
        assert!(text.contains("## 1970-01-01\n"));
        assert!(text.contains("- **Added** [[Person/Ada]]\n"));
        assert!(text.contains("- **Changed** [[Person/Grace]]\n"));
        assert!(text.contains("- **Removed** Person/Old.md\n"));
    }

    #[test]
    fn a_root_file_lists_its_bare_path_in_the_changelog() {
        // A root file's vault key is `Vault/Notes`, so the path minus
        // `.md` is not a planned key — it renders bare, never linked.
        let changes = vec![Change::Added("Notes.md".into())];
        let keys = BTreeSet::from(["Vault/Notes"]);
        let text = changelog(&changes, &keys, 0);
        assert!(text.contains("- **Added** Notes.md\n"));
        assert!(!text.contains("[[Notes]]"));
    }

    #[test]
    fn root_and_label_indexes_list_the_labels() {
        let labels = BTreeMap::from([(
            "Person".to_string(),
            vec!["Person/Ada".to_string(), "Person/Grace".to_string()],
        )]);
        let root = root_index(&labels);
        assert!(root.contains("- **Person** (2) — see [[Person/index]]\n"));
        let label = label_index("Person", &labels["Person"]);
        assert!(label.contains("# Person\n\n"));
        assert!(label.contains("- [[Person/Ada]]\n"));
    }
}
