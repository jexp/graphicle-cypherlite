//! Vault link targets (ADR-0406 §2): wikilinks are the link form,
//! native Obsidian syntax (resolves in the graph view). **Two spellings
//! parse; one is written.** The codec writes only the full form
//! `[[Label/<key>]]`; on load the short form `[[Note]]` — the spelling
//! real Obsidian vaults use — parses too, resolved by the codec through
//! the learned schema's tables. An optional `|display alias` suffix —
//! Obsidian's display separator — is accepted and stripped.

use std::fmt;

/// A parsed link target: the full form names one node file by primary
/// label and key (`key` carries `/` separators for files nested deeper
/// than the label directory); the short form names only a key, resolved
/// by the codec.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LinkTarget {
    Full { label: String, key: String },
    Short(String),
}

impl fmt::Display for LinkTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full { label, key } => write!(f, "[[{label}/{key}]]"),
            Self::Short(name) => write!(f, "[[{name}]]"),
        }
    }
}

/// Parse one wikilink string: `[[Label/key]]` or `[[Note]]`, with an
/// optional `|display alias`. Returns `None` for anything else.
#[must_use]
pub fn parse_link(raw: &str) -> Option<LinkTarget> {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))?;
    // An optional `|display alias` suffix: everything before the first
    // separator is the target; `None` means no alias — the whole inner
    // text is the target.
    let target = match inner.split_once('|') {
        Some((before, _)) => before,
        None => inner,
    }
    .trim();
    if target.is_empty() {
        return None;
    }
    match target.split_once('/') {
        Some((label, key)) => {
            if label.is_empty() || key.split('/').any(str::is_empty) {
                return None;
            }
            Some(LinkTarget::Full {
                label: label.to_string(),
                key: key.to_string(),
            })
        }
        None => Some(LinkTarget::Short(target.to_string())),
    }
}

/// Render the canonical written form: `[[Label/key]]` (YAML quoting is
/// the emitter's job; this is the raw string inside it). A short form
/// cannot be written — the codec resolves it first.
pub(crate) fn render_wikilink(target: &LinkTarget) -> Result<String, cypherlite_core::Error> {
    match target {
        // The written form is `Display`'s one spelling of the full form.
        LinkTarget::Full { .. } => Ok(target.to_string()),
        LinkTarget::Short(name) => Err(cypherlite_core::Error::storage(format!(
            "the short-form wikilink [[{name}]] cannot be written; resolve it first"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::LinkTarget;

    fn full(label: &str, key: &str) -> LinkTarget {
        LinkTarget::Full {
            label: label.into(),
            key: key.into(),
        }
    }

    #[test]
    fn parses_wikilinks_with_and_without_alias() {
        assert_eq!(
            parse_link("[[Person/Grace]]"),
            Some(full("Person", "Grace"))
        );
        assert_eq!(
            parse_link("[[Place/Ithaca|home]]"),
            Some(full("Place", "Ithaca"))
        );
    }

    #[test]
    fn short_form_parses_full_form_writes() {
        assert_eq!(
            parse_link("[[Grace]]"),
            Some(LinkTarget::Short("Grace".into()))
        );
        assert_eq!(
            parse_link("[[Grace Hopper]]"),
            Some(LinkTarget::Short("Grace Hopper".into()))
        );
        // The written form is always full.
        assert_eq!(
            render_wikilink(&full("Person", "Grace")).unwrap(),
            "[[Person/Grace]]"
        );
        assert!(render_wikilink(&LinkTarget::Short("Grace".into())).is_err());
    }

    #[test]
    fn rejects_malformed_links() {
        assert_eq!(parse_link("Grace"), None);
        assert_eq!(parse_link("[[]]"), None);
        assert_eq!(parse_link("[[Person/]]"), None);
        assert_eq!(parse_link("[[Person//Grace]]"), None);
        assert_eq!(parse_link("/Place/Ithaca.md"), None);
    }

    #[test]
    fn nested_key_paths_round_trip() {
        let target = full("Notes", "2026/october/plan");
        assert_eq!(parse_link(&render_wikilink(&target).unwrap()), Some(target));
    }

    #[test]
    fn targets_order_full_before_short_by_content() {
        let a = full("A", "z");
        let b = full("B", "a");
        assert!(a < b);
    }
}
