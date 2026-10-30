//! Architecture rule documents: the authoring surface of the architecture
//! layer (architecture-concepts.md §1.5).
//!
//! A rule document is prose plus rule blocks. Two frontends read the same
//! rule grammar into one shared model:
//!
//! - [`markdown`]: a Markdown file whose rules are four-backtick fences
//!   (`docs/architecture-rules.md`).
//! - [`adoc`]: an AsciiDoc file in jQAssistant's convention, with
//!   `[source,cypher,role=concept]` listings and `include::` (see the
//!   compatibility table in `docs/architecture-rules.md`).
//!
//! [`model`] holds the rule structs and every rule check, so the frontends
//! cannot disagree about what a rule means. [`store`] writes the parsed rules
//! as `(:ConceptRule)`, `(:ConstraintRule)` and `(:RuleGroup)` nodes.
//! [`executor`] applies the stored concept rules to a recorded episode, as
//! the `enrich` job (architecture-concepts.md §1 and §5).
use crate::Result as GraphResult;
use crate::store_graph::GraphStore;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub mod adoc;
pub mod executor;
pub mod markdown;
pub mod model;
pub mod store;

pub use executor::{enrich, has_rules, rollback};
pub use model::{Group, Reason, Rule, RuleDocument, RuleError, Severity};
pub use store::{rule_set_hash, store};

/// Parse a Markdown rule document. This is the frontend the earlier
/// Markdown-only entry point named `parse`; it stays under that name so
/// existing callers keep compiling.
pub fn parse(text: &str) -> Result<RuleDocument, RuleError> {
    parse_markdown(text)
}

/// Parse a Markdown rule document.
pub fn parse_markdown(text: &str) -> Result<RuleDocument, RuleError> {
    markdown::parse(text)
}

/// Parse an AsciiDoc rule document. `base` is the folder its `include::`
/// targets are read from.
pub fn parse_adoc(text: &str, base: Option<&Path>) -> Result<RuleDocument, RuleError> {
    adoc::parse(text, base)
}

/// Parse a rule document read from `path`, choosing the frontend by its
/// extension: `.md` and `.markdown` are Markdown, `.adoc`, `.asciidoc` and
/// `.asc` are AsciiDoc. Includes resolve from the file's folder.
pub fn parse_file(path: &Path) -> Result<RuleDocument, RuleError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        RuleError::new(
            "",
            None,
            0,
            Reason::Invalid(format!("cannot read {}: {error}", path.display())),
        )
    })?;
    parse_text(path, &text)
}

/// Parse the text of a rule document that lives at `path` (the path names the
/// file for errors and the frontend, and its folder resolves includes).
pub fn parse_text(path: &Path, text: &str) -> Result<RuleDocument, RuleError> {
    let file = path.display().to_string();
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let parsed = match extension.as_str() {
        "md" | "markdown" => parse_markdown(text),
        "adoc" | "asciidoc" | "asc" => parse_adoc(text, path.parent()),
        _ => Err(RuleError::new(
            "",
            None,
            0,
            Reason::Invalid(format!(
                "{file} is not a rule document: expected .md or .adoc"
            )),
        )),
    };
    parsed.map_err(|error| error.in_file_if_unset(Some(&file)))
}

/// The folders a project's rule documents are read from at sync, and the
/// extensions read there (architecture-concepts.md §1.5). The ingested path
/// patterns are these folders crossed with these extensions.
pub const FOLDERS: &[&str] = &["docs/architecture", ".graphicle"];
pub const EXTENSIONS: &[&str] = &["md", "adoc"];

/// The rule documents a project folder holds: the files directly in each
/// [`FOLDERS`] entry with an [`EXTENSIONS`] extension, as folder-relative
/// `(path, file)` sorted by path. Symlinks and folders are not followed.
fn documents_in(root: &Path) -> Vec<(String, PathBuf)> {
    let mut found = BTreeMap::new();
    for folder in FOLDERS {
        let Ok(entries) = std::fs::read_dir(root.join(folder)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let extension = Path::new(&name)
                .extension()
                .and_then(|value| value.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            if !EXTENSIONS.contains(&extension.as_str())
                || !std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file())
            {
                continue;
            }
            found.insert(format!("{folder}/{name}"), path);
        }
    }
    found.into_iter().collect()
}

/// The text of a rule document of at most `limit` bytes that is UTF-8.
fn read_text(path: &Path, limit: usize) -> std::result::Result<String, String> {
    let meta =
        std::fs::symlink_metadata(path).map_err(|error| format!("Could not read: {error}"))?;
    if meta.len() > limit as u64 {
        return Err(format!("Larger than {limit} bytes"));
    }
    let bytes = std::fs::read(path).map_err(|error| format!("Could not read: {error}"))?;
    String::from_utf8(bytes).map_err(|_| "Not UTF-8 text".to_string())
}

/// The sync's rule document pass, run after the sync's commit. A document whose
/// text digest differs from the recorded one is parsed again and its rules
/// replaced; an unchanged one is not parsed; a document that is gone loses its
/// rules and marker. A document that parses (to zero rules or more) records its
/// digest, so an empty one is not re-parsed each sync. A document that cannot
/// be read or does not parse is listed under `skipped` and its old rules are
/// removed. With no document in the folder and none recorded, nothing is
/// written and the result is `None`.
pub fn ingest(
    graph: &GraphStore,
    repository_id: &str,
    root: &Path,
    limit: usize,
) -> GraphResult<Option<Value>> {
    let found = documents_in(root);
    let recorded = store::recorded_documents(graph, repository_id)?;
    if found.is_empty() && recorded.is_empty() {
        return Ok(None);
    }
    let present: BTreeSet<&str> = found.iter().map(|(source, _)| source.as_str()).collect();
    let (mut parsed_count, mut removed) = (0usize, 0usize);
    let mut skipped = Vec::new();
    for source in recorded
        .keys()
        .filter(|source| !present.contains(source.as_str()))
    {
        store::replace_document(graph, repository_id, source, None)?;
        removed += 1;
    }
    for (source, path) in &found {
        let had = recorded.contains_key(source);
        let text = match read_text(path, limit) {
            Ok(text) => text,
            Err(reason) => {
                skipped.push(json!({"path": source, "reason": reason}));
                if had {
                    store::replace_document(graph, repository_id, source, None)?;
                    removed += 1;
                }
                continue;
            }
        };
        let sha = graphicle_memory::text::sha256(text.as_bytes());
        if recorded.get(source) == Some(&sha) {
            continue;
        }
        match parse_text(path, &text) {
            Ok(document) => {
                store::replace_document(graph, repository_id, source, Some((&sha, &document)))?;
                parsed_count += 1;
            }
            Err(error) => {
                skipped.push(json!({"path": source, "reason": error.to_string()}));
                if had {
                    store::replace_document(graph, repository_id, source, None)?;
                    removed += 1;
                }
            }
        }
    }
    Ok(Some(json!({
        "documents": found.len(),
        "parsed": parsed_count,
        "removed": removed,
        "rule_set": rule_set_hash(graph, repository_id)?,
        "skipped": skipped,
    })))
}

#[cfg(test)]
mod tests;
