//! Markdown vault document grammar for the markdown snapshot codec
//! (ADR-0406).
//!
//! A vault node is one markdown file, `<vault>/<Label>/<key>.md`, whose
//! YAML frontmatter carries the node's properties and its outgoing
//! relationship links (a key is a link list iff it names a relationship
//! type in the caller's vocabulary — classification is schema-driven,
//! never shape-sniffed) and whose body is the text of the node's ordered
//! body properties (ADR-0460): the first present one renders bare, each
//! subsequent one as a `## <property>` section — `["body"]`, the bare
//! single property, when nothing is configured.
//! The layout targets OKF v0.2 and stays Obsidian-native: `type` is the
//! one required frontmatter key, wikilinks `[[Label/<key>]]` resolve in
//! Obsidian's graph view, reserved view files (`index.md`, `log.md`)
//! are never concept nodes.
//!
//! This crate is the pure grammar: values move as the engine's
//! canonical tagged JSON values (`cypherlite_model::serde_value`), so
//! the storage codec converts properties once and this crate never
//! re-derives the value vocabulary. It is deliberately dependency-light
//! (`cypherlite-core` for the shared `Error`, `serde_json` for values,
//! `saphyr-parser` for YAML): the VFS, the WAL and the graph stay in
//! `cypherlite-storage`.
//!
//! Determinism is part of the format contract (ADR-0406 §2): the same
//! graph always renders byte-identical files — frontmatter keys in a
//! fixed order, relationship entries sorted and deduplicated (set
//! semantics).
//!
//! ```
//! use cypherlite_markdown::{parse_link, LinkEntry, NodeDoc};
//! use serde_json::Value;
//! use std::collections::{BTreeMap, BTreeSet};
//!
//! let rel_types = BTreeSet::from(["KNOWS".to_string()]);
//! // The label's ordered body-property list (ADR-0460); `["body"]` is
//! // the default single-property form.
//! let body_props = vec!["body".to_string()];
//! let doc = NodeDoc {
//!     primary_label: "Person".into(),
//!     extra_labels: vec![],
//!     properties: BTreeMap::from([
//!         ("name".to_string(), Value::String("Ada".into())),
//!         ("body".to_string(), Value::String("Analytical engine pioneer.".into())),
//!     ]),
//!     links: BTreeMap::from([(
//!         "KNOWS".to_string(),
//!         vec![LinkEntry {
//!             target: parse_link("[[Person/Grace]]").unwrap(),
//!             properties: BTreeMap::from([(
//!                 "since".to_string(),
//!                 Value::Number(2020.into()),
//!             )]),
//!         }],
//!     )]),
//! };
//! let text = cypherlite_markdown::render_node_doc(&doc, &rel_types, &body_props).unwrap();
//! assert!(text.starts_with("---\ntype: Person\nname: Ada\nKNOWS:\n"));
//! assert!(text.contains("  - since: 2020\n    target: \"[[Person/Grace]]\"\n"));
//! assert!(text.ends_with("Analytical engine pioneer.\n"));
//!
//! let parsed = cypherlite_markdown::parse_node_doc(
//!     "Person", "Person/Ada.md", &text, &rel_types, &body_props,
//! ).unwrap();
//! assert_eq!(parsed, doc);
//! ```

mod body;
mod doc;
mod link;
mod name;
mod yaml;

use cypherlite_core::Error;

/// The crate's one `markdown vault: ` error-prefix policy — every
/// grammar module reports through this helper instead of re-spelling the
/// prefix (doc-level and name-level errors share it; the YAML and body
/// modules own their own, differently-scoped prefixes). The storage
/// codec's `codec_error` delegates here too, so the prefix has one home
/// across both crates.
pub fn vault_error(message: impl Into<String>) -> Error {
    Error::storage(format!("markdown vault: {}", message.into()))
}

/// The C0 control characters (U+0000–U+001F) plus DEL (U+007F): the one
/// spelling of the control set, shared by the name sanitizer and the
/// YAML string emitters. `char::is_ascii_control` alone stops at U+001F
/// — std documents that DEL is not an ASCII control character — so the
/// DEL arm is spelled here, once.
pub(crate) fn is_c0_del(ch: char) -> bool {
    ch.is_ascii_control() || ch == '\u{7f}'
}

// The public surface is exactly what the single consumer
// (cypherlite-storage's markdown codec) imports, plus the doctest's
// entry points; nothing is exported speculatively.
pub use body::fold_case;
pub use body::{parse_body, BODY_KEY};
pub use doc::{
    is_reserved_view_file_name, parse_node_doc, parse_node_doc_root, render_node_doc,
    validate_body_properties, LinkEntry, NodeDoc, INDEX_VIEW, KEY_TARGET, LOG_VIEW,
};
pub use link::{parse_link, LinkTarget};
pub use name::{sanitize_label, sanitize_name_path};
pub use yaml::{parse_yaml_value, split_frontmatter};

/// The one body-property-list constructor the crate's test modules
/// share (the body and doc suites both build lists from name slices).
#[cfg(test)]
pub(crate) fn body_props(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}
