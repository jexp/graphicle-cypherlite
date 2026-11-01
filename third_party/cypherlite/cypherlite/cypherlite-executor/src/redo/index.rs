//! The index-DDL payload family: the typed index kind, the serving-pair
//! predicates, the legacy property encoding, and the detail spec its
//! `CatalogIndexDetail` op carries. A private sibling re-exported at the
//! `redo` root (the one-public-path pattern), so callers keep using
//! `crate::redo::…`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::RedoOp;

/// The DDL index kind carried by [`IndexDetailSpec`]: a
/// serde-serializable mirror of `cypherlite_parser::IndexKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexKindSpec {
    Range,
    Point,
    Text,
    Fulltext,
    Lookup,
    Vector,
}

impl IndexKindSpec {
    /// Every variant; derived inverses (and their tests) iterate this so
    /// the variant list is maintained in exactly one place.
    pub const ALL: [IndexKindSpec; 6] = [
        IndexKindSpec::Range,
        IndexKindSpec::Point,
        IndexKindSpec::Text,
        IndexKindSpec::Fulltext,
        IndexKindSpec::Lookup,
        IndexKindSpec::Vector,
    ];

    /// The upper-case display name Neo4j's `SHOW INDEXES` `type` column
    /// uses (same strings as `cypherlite_parser::IndexKind::display_name`).
    /// The `SHOW INDEXES` `type` column spelling — the parser's own display
    /// name, so the six names live once in the crate that owns the dialect.
    pub fn display_name(&self) -> &'static str {
        cypherlite_parser::IndexKind::from(*self).display_name()
    }

    /// Parse the display name back into the kind — the inverse of
    /// [`IndexKindSpec::display_name`], used to recover the kind of
    /// legacy catalog rows that predate `CatalogIndexDetail` from their
    /// `{props}#{KIND}#{name}` property encoding. Derived from the one
    /// string table in `display_name` so the two directions cannot drift.
    pub fn from_display_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.display_name() == name)
    }

    /// The Neo4j `SHOW INDEXES` `indexProvider` column value for this
    /// kind. Exhaustive on purpose: a new index kind fails to compile
    /// here instead of silently falling back to `range-1.0`.
    pub fn provider_name(&self) -> &'static str {
        match self {
            IndexKindSpec::Range => "range-1.0",
            IndexKindSpec::Point => "point-1.0",
            IndexKindSpec::Text => "text-3.0",
            IndexKindSpec::Fulltext => "fulltext-1.0",
            IndexKindSpec::Lookup => "token-lookup-1.0",
            IndexKindSpec::Vector => "vector-3.0",
        }
    }
}

impl From<IndexKindSpec> for cypherlite_parser::IndexKind {
    fn from(kind: IndexKindSpec) -> Self {
        match kind {
            IndexKindSpec::Range => cypherlite_parser::IndexKind::Range,
            IndexKindSpec::Point => cypherlite_parser::IndexKind::Point,
            IndexKindSpec::Text => cypherlite_parser::IndexKind::Text,
            IndexKindSpec::Fulltext => cypherlite_parser::IndexKind::Fulltext,
            IndexKindSpec::Lookup => cypherlite_parser::IndexKind::Lookup,
            IndexKindSpec::Vector => cypherlite_parser::IndexKind::Vector,
        }
    }
}

impl From<cypherlite_parser::IndexKind> for IndexKindSpec {
    fn from(kind: cypherlite_parser::IndexKind) -> Self {
        match kind {
            cypherlite_parser::IndexKind::Range => IndexKindSpec::Range,
            cypherlite_parser::IndexKind::Point => IndexKindSpec::Point,
            cypherlite_parser::IndexKind::Text => IndexKindSpec::Text,
            cypherlite_parser::IndexKind::Fulltext => IndexKindSpec::Fulltext,
            cypherlite_parser::IndexKind::Lookup => IndexKindSpec::Lookup,
            cypherlite_parser::IndexKind::Vector => IndexKindSpec::Vector,
        }
    }
}

/// The index side of an ADR-0372 serving-pair check: the rule's kind,
/// entity kind, labels, and properties, as the creating statement spells
/// them (or as the materialized `index_details` record them).
#[derive(Clone, Copy)]
pub struct ServingPairIndex<'a> {
    pub kind: IndexKindSpec,
    pub relationship: bool,
    pub labels: &'a [String],
    pub properties: &'a [String],
}

/// The constraint side of an ADR-0372 serving-pair check.
#[derive(Clone, Copy)]
pub struct ServingPairConstraint<'a> {
    pub backed: bool,
    pub relationship: bool,
    pub label: &'a str,
    pub properties: &'a [String],
}

/// ADR-0372's serving-pair conflict, the one canonical form, owning the
/// whole match: the pair's typed fields in, the verdict out. A schema rule
/// pair conflicts only when the index kind is RANGE — the only kind that
/// can serve a constraint's backing (ADR-0339) — and the constraint is
/// index-backed (uniqueness/key), on the same label and properties.
/// Property-type/existence constraints and FULLTEXT/VECTOR/POINT indexes
/// coexist with everything. All three creation directions consult this; a
/// change to the rule lands here once.
pub fn serving_pair_conflicts(
    index: ServingPairIndex<'_>,
    constraint: ServingPairConstraint<'_>,
) -> bool {
    index.kind == IndexKindSpec::Range
        && index.relationship == constraint.relationship
        && index.labels.len() == 1
        && constraint.label == index.labels[0]
        && constraint.properties == index.properties
        && constraint.backed
}

/// Decode a legacy `IndexDef.property` string: split the value `catalog_create_index`
/// (and the constraint-backed index
/// path) into its props head and index kind. A bare property name (no
/// `#`) is its own head with no kind. The single decoder for this
/// format — every reader of `IndexDef.property` goes through here.
pub fn split_legacy_property_encoding(property: &str) -> (&str, Option<IndexKindSpec>) {
    match property.split_once('#') {
        None => (property, None),
        Some((head, rest)) => (
            head,
            rest.split('#')
                .next()
                .and_then(IndexKindSpec::from_display_name),
        ),
    }
}

/// Encode an `IndexDef.property` in the legacy `{property}#{KIND}#{name}`
/// format — the encoder half of [`split_legacy_property_encoding`], which
/// decodes the result back to `(property, Some(kind))`.
pub fn legacy_property_encoding(property: &str, kind: IndexKindSpec, name: &str) -> String {
    format!("{property}#{}#{name}", kind.display_name())
}

/// Typed payload of [`RedoOp::CatalogIndexDetail`]: the catalog facts of
/// a `CREATE INDEX`, captured once at DDL-execution time from the parsed
/// clause. `apply_redo` stores it verbatim -- it never formats a CREATE
/// INDEX string and re-parses it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexDetailSpec {
    pub kind: IndexKindSpec,
    /// True for a relationship (`FOR ()-[r:T]-()`) index.
    pub relationship: bool,
    /// Node labels, or (for a relationship index) relationship types;
    /// more than one entry only for the `A|B` disjunctive form, empty for
    /// a LOOKUP index.
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub properties: Vec<String>,
    /// CIP-235 `WITH [...]` single-stage-filtering properties (VECTOR).
    #[serde(default)]
    pub with_properties: Vec<String>,
}

impl IndexDetailSpec {
    /// Node `(label, property)` pairs this detail materializes and charges.
    /// Relationship indexes, multi-label indexes, and kinds other than
    /// single-property TEXT or POINT, or RANGE, contribute nothing.
    pub fn covered_pairs(&self) -> Vec<(&str, &str, IndexKindSpec)> {
        if self.relationship || self.labels.len() != 1 || self.properties.is_empty() {
            return Vec::new();
        }
        let label = self.labels[0].as_str();
        match self.kind {
            IndexKindSpec::Text | IndexKindSpec::Point if self.properties.len() == 1 => {
                vec![(label, self.properties[0].as_str(), self.kind)]
            }
            IndexKindSpec::Range => self
                .properties
                .iter()
                .map(|property| (label, property.as_str(), IndexKindSpec::Range))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The node `(label, property)` pairs this index serves an equality seek
    /// on. Derived from [`Self::covered_pairs`] so the entity, label-count
    /// and kind gates live once: only a single-label node RANGE index
    /// contributes, and it contributes **every** covered property — the
    /// executor materializes and seeks each component of a composite index,
    /// so leading-only coverage would under-report it. A relationship index
    /// never yields a node pair, and a property name is never split on a
    /// comma.
    pub(crate) fn range_seek_pairs(&self) -> Vec<(&str, &str)> {
        self.covered_pairs()
            .into_iter()
            .filter_map(|(label, property, kind)| {
                (kind == IndexKindSpec::Range).then_some((label, property))
            })
            .collect()
    }
}

/// Apply the catalog index ops in `ops` to `details`. This is the speculative
/// projection `estimate_redo_delta` uses; committed replay in `apply_redo`
/// stores the same two ops.
pub fn project_index_details(details: &mut BTreeMap<String, IndexDetailSpec>, ops: &[RedoOp]) {
    for op in ops {
        match op {
            RedoOp::CatalogIndexDetail { name, detail, .. } => {
                details.insert(name.clone(), detail.clone());
            }
            RedoOp::DropIndex { name } => {
                details.remove(name);
            }
            _ => {}
        }
    }
}
