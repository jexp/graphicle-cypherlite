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
use std::path::Path;

pub mod adoc;
pub mod markdown;
pub mod model;
pub mod store;

pub use model::{Group, Reason, Rule, RuleDocument, RuleError, Severity};
pub use store::store;

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
    let file = path.display().to_string();
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let parsed = match extension.as_str() {
        "md" | "markdown" => parse_markdown(&text),
        "adoc" | "asciidoc" | "asc" => parse_adoc(&text, path.parent()),
        _ => Err(RuleError::new(
            "",
            None,
            0,
            Reason::Invalid(format!("{file} is not a rule document: expected .md or .adoc")),
        )),
    };
    parsed.map_err(|error| error.in_file_if_unset(Some(&file)))
}

#[cfg(test)]
mod tests;
