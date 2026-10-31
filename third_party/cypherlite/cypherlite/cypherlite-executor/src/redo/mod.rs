//! Physical redo ops for the WAL (ADR-0012). Not Cypher text.
//!
//! Catalog-DDL ops (`CatalogIndexDetail`, `PutGraphType`,
//! `SetCurrentGraphType`) carry typed, serde-serializable payload models
//! captured once at DDL-execution time. `Graph::apply_redo` stores them
//! verbatim; it never re-parses Cypher text and never swallows a decode
//! failure.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use cypherlite_core::{NodeId, RelId, Result, Value};

use crate::graph_type::GraphTypeSpec;

// Redo ops carry typed values; the tagged-JSON encoding is canonical on
// `Value` in cypherlite-core (shared with the Parquet codec). The one cold
// path that still carries a raw `serde_json::Value` (`CatalogIndexDetail.options`)
// uses the helpers in `wal_format_json`. Re-exported so callers stay unchanged.
pub mod wal_format_json;
pub use wal_format_json::{props_from_json, props_json};

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
    pub fn display_name(&self) -> &'static str {
        match self {
            IndexKindSpec::Range => "RANGE",
            IndexKindSpec::Point => "POINT",
            IndexKindSpec::Text => "TEXT",
            IndexKindSpec::Fulltext => "FULLTEXT",
            IndexKindSpec::Lookup => "LOOKUP",
            IndexKindSpec::Vector => "VECTOR",
        }
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

/// written by `catalog_create_index` (and the constraint-backed index
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum RedoOp {
    CreateNode {
        id: NodeId,
        labels: std::sync::Arc<Vec<String>>,
        properties: std::sync::Arc<cypherlite_core::PropertyMap>,
    },
    CreateRel {
        id: RelId,
        start: NodeId,
        end: NodeId,
        rel_type: String,
        properties: std::sync::Arc<cypherlite_core::PropertyMap>,
    },
    DeleteNode {
        id: NodeId,
    },
    DeleteRel {
        id: RelId,
    },
    SetNodeProp {
        id: NodeId,
        key: String,
        value: Value,
    },
    RemoveNodeProp {
        id: NodeId,
        key: String,
    },
    SetRelProp {
        id: RelId,
        key: String,
        value: Value,
    },
    RemoveRelProp {
        id: RelId,
        key: String,
    },
    AddLabels {
        id: NodeId,
        labels: Vec<String>,
    },
    RemoveLabels {
        id: NodeId,
        labels: Vec<String>,
    },
    CreateIndex {
        name: String,
        label: String,
        property: String,
    },
    DropIndex {
        name: String,
    },
    CreateConstraint {
        name: String,
        #[serde(default)]
        relationship: bool,
        label: String,
        properties: Vec<String>,
        kind: String,
        #[serde(default)]
        property_type: Option<String>,
    },
    DropConstraint {
        name: String,
    },
    CatalogIndexDetail {
        name: String,
        detail: IndexDetailSpec,
        /// The evaluated `OPTIONS` map (tagged JSON values, see
        /// [`props_json`]). Decoded strictly by [`props_from_json`].
        options: serde_json::Value,
    },
    /// SHOW metadata for an explicitly enforced constraint.
    CatalogDerivedConstraint {
        constraint: cypherlite_model::Constraint,
    },
    PutDatabase {
        name: String,
        language: u32,
        composite: bool,
    },
    /// Legacy, decode-only: `DROP DATABASE` was executable before the
    /// parser failed closed on unsupported administration commands
    /// (ADR-0116), so WALs and snapshots written by those versions may
    /// carry this op. New code never constructs it; replay is the
    /// ADR-0116 §8 no-op (only fixed namespaces exist, and a fixed
    /// namespace can never be dropped). Removing the variant would make
    /// serde reject the old record and hard-fail store open (ADR-0114).
    DropDatabase {
        name: String,
    },
    PutGraphType {
        name: String,
        body: GraphTypeSpec,
    },
    SetCurrentGraphType {
        body: GraphTypeSpec,
    },
    /// Source-scoped pull identity (ADR-0080). Not a graph CDC event.
    PullIdentityPut {
        source: String,
        kind: PullKind,
        remote_id: String,
        local_id: i64,
    },
    /// Store-level ontology document ops (ADR-0120 §3). Not graph ops:
    /// they carry the validated ontology document verbatim (at most one
    /// per store, for `neo4j`) and mutate no graph or catalog state; the
    /// derived `system_ontologies` graph is rebuilt from the document.
    /// The document is bounded by [`MAX_ONTOLOGY_SPEC_BYTES`], checked
    /// before the commit is written.
    OntologySet {
        database: String,
        spec: String,
    },
    OntologyDrop {
        database: String,
    },
}

/// The ADR-0086 object envelope bounding one ontology document
/// (ADR-0120 §3): 16 MiB, checked before the commit is written.
pub const MAX_ONTOLOGY_SPEC_BYTES: usize = 16 * 1024 * 1024;

/// Typed view of a store-level ontology document op (ADR-0120 §3), the
/// classification [`RedoOp::ontology_op`] returns: `Set` borrows the
/// verbatim document, `Drop` carries nothing. Consumers match this once
/// instead of re-matching the redo variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OntologyOp<'a> {
    /// `OntologySet`: the verbatim ontology document.
    Set(&'a str),
    /// `OntologyDrop`.
    Drop,
}

impl RedoOp {
    pub fn create_node(
        id: NodeId,
        labels: impl Into<std::sync::Arc<Vec<String>>>,
        properties: impl Into<cypherlite_core::PropertyMap>,
    ) -> Self {
        Self::CreateNode {
            id,
            labels: labels.into(),
            properties: std::sync::Arc::new(properties.into()),
        }
    }

    pub fn create_rel(
        id: RelId,
        start: NodeId,
        end: NodeId,
        rel_type: String,
        properties: impl Into<cypherlite_core::PropertyMap>,
    ) -> Self {
        Self::CreateRel {
            id,
            start,
            end,
            rel_type,
            properties: std::sync::Arc::new(properties.into()),
        }
    }

    pub fn delete_node(id: NodeId) -> Self {
        Self::DeleteNode { id }
    }

    pub fn delete_rel(id: RelId) -> Self {
        Self::DeleteRel { id }
    }

    pub fn add_labels(id: NodeId, labels: Vec<String>) -> Self {
        Self::AddLabels { id, labels }
    }

    pub fn remove_labels(id: NodeId, labels: Vec<String>) -> Self {
        Self::RemoveLabels { id, labels }
    }

    pub fn set_node_prop(id: NodeId, key: String, value: Value) -> Self {
        Self::SetNodeProp { id, key, value }
    }

    pub fn remove_node_prop(id: NodeId, key: String) -> Self {
        Self::RemoveNodeProp { id, key }
    }

    pub fn set_rel_prop(id: RelId, key: String, value: Value) -> Self {
        Self::SetRelProp { id, key, value }
    }

    pub fn remove_rel_prop(id: RelId, key: String) -> Self {
        Self::RemoveRelProp { id, key }
    }

    /// The typed ontology classification of this op (ADR-0120 §3); `None`
    /// for every non-ontology op. One match — [`Self::is_ontology_op`],
    /// the WAL's `MDBWAL02` segment-magic rule (ADR-0120 §4), the
    /// scanner's corruption check and the facade's commit scan all derive
    /// from it.
    pub fn ontology_op(&self) -> Option<OntologyOp<'_>> {
        match self {
            Self::OntologySet { spec, .. } => Some(OntologyOp::Set(spec)),
            Self::OntologyDrop { .. } => Some(OntologyOp::Drop),
            _ => None,
        }
    }

    /// Whether this is a store-level ontology document op (ADR-0120 §3).
    /// The WAL uses it for the `MDBWAL02` segment-magic rule (ADR-0120 §4)
    /// and the scanner for the corruption check; the commit-preflight seam
    /// uses it for the document-size gate.
    pub fn is_ontology_op(&self) -> bool {
        self.ontology_op().is_some()
    }

    /// The one pre-persist validation entry point, run before writing or
    /// applying redo: every persisted local ID is in range (the upper
    /// bound is reserved because installation advances the ID allocator),
    /// and an `OntologySet` document is within the 16 MiB ADR-0086 object
    /// envelope (ADR-0120 §3). Non-ontology ops always pass the size arm.
    pub fn validate_ids(&self) -> Result<()> {
        fn check(id: i64, what: &str) -> Result<()> {
            cypherlite_core::validate_persisted_id(id, what)
        }
        match self {
            Self::CreateNode { id, .. }
            | Self::DeleteNode { id }
            | Self::SetNodeProp { id, .. }
            | Self::RemoveNodeProp { id, .. }
            | Self::AddLabels { id, .. }
            | Self::RemoveLabels { id, .. } => check(*id, "WAL node"),
            Self::CreateRel { id, start, end, .. } => {
                check(*id, "WAL relationship")?;
                check(*start, "WAL relationship start")?;
                check(*end, "WAL relationship end")
            }
            Self::DeleteRel { id }
            | Self::SetRelProp { id, .. }
            | Self::RemoveRelProp { id, .. } => check(*id, "WAL relationship"),
            Self::PullIdentityPut { local_id, .. } => check(*local_id, "WAL pull identity local"),
            // ADR-0120 §3: the ontology document is limited to 16 MiB
            // (the ADR-0086 object envelope), checked before the commit is
            // written.
            Self::OntologySet { spec, .. } => {
                if spec.len() > MAX_ONTOLOGY_SPEC_BYTES {
                    return Err(cypherlite_core::Error::storage(format!(
                        "ontology document of {} bytes exceeds the 16 MiB limit",
                        spec.len()
                    )));
                }
                Ok(())
            }
            Self::CreateIndex { .. }
            | Self::DropIndex { .. }
            | Self::CreateConstraint { .. }
            | Self::DropConstraint { .. }
            | Self::CatalogIndexDetail { .. }
            | Self::CatalogDerivedConstraint { .. }
            | Self::PutDatabase { .. }
            | Self::DropDatabase { .. }
            | Self::PutGraphType { .. }
            | Self::SetCurrentGraphType { .. }
            | Self::OntologyDrop { .. } => Ok(()),
        }
    }

    /// Whether this is a catalog op: anything but the ten entity ops,
    /// `PullIdentityPut` and the store-level ontology ops (ADR-0120 §3).
    /// The write overlay replays only these when it resolves its
    /// speculative schema (ADR-0172 §8). Classified once, by
    /// [`Self::write_target`] (ADR-0173).
    pub(crate) fn is_catalog(&self) -> bool {
        self.write_target().is_catalog()
    }

    /// What this op writes, for ADR-0015 conflict classification. Total:
    /// every variant classifies, so a future op is a compile error here,
    /// not a silently missed conflict rule; consumers bind every payload
    /// from the one classification. Crate-internal: the consumer set is
    /// `graph::conflict`; re-export if a cross-crate consumer appears.
    pub(crate) fn write_target(&self) -> WriteTarget<'_> {
        match self {
            Self::CreateNode { id, .. } => WriteTarget::CreatedNode(*id),
            Self::CreateRel { id, start, end, .. } => WriteTarget::CreatedRel {
                id: *id,
                start: *start,
                end: *end,
            },
            Self::DeleteNode { id } => WriteTarget::DeletedNode(*id),
            Self::DeleteRel { id } => WriteTarget::DeletedRel(*id),
            Self::SetNodeProp { id, .. }
            | Self::RemoveNodeProp { id, .. }
            | Self::AddLabels { id, .. }
            | Self::RemoveLabels { id, .. } => WriteTarget::MutatedNode(*id),
            Self::SetRelProp { id, .. } | Self::RemoveRelProp { id, .. } => {
                WriteTarget::MutatedRel(*id)
            }
            Self::CreateIndex {
                name,
                label,
                property,
            } => WriteTarget::CreateIndex {
                name,
                label,
                property,
            },
            Self::DropIndex { name } => WriteTarget::DropIndex { name },
            Self::CreateConstraint { name, .. } => WriteTarget::CreateConstraint { name },
            Self::DropConstraint { name } => WriteTarget::DropConstraint { name },
            Self::CatalogIndexDetail { .. }
            | Self::CatalogDerivedConstraint { .. }
            | Self::PutDatabase { .. }
            | Self::DropDatabase { .. }
            | Self::PutGraphType { .. }
            | Self::SetCurrentGraphType { .. } => WriteTarget::CatalogMeta,
            Self::PullIdentityPut { .. } => WriteTarget::PullIdentity,
            Self::OntologySet { .. } | Self::OntologyDrop { .. } => WriteTarget::Ontology,
        }
    }

    /// Every catalog name this op mentions — a label, a relationship type or
    /// a property key — for the write overlay's catalog facts (ADR-0363
    /// decision 5): the redo log is the scratch's one write record, so a name
    /// a transaction has mentioned is known before the commit publishes it.
    ///
    /// Total over the enum: every variant classifies, and a new variant is a
    /// compile error here, never a silently skipped name. Monotone, like the
    /// committed catalog: an op that removes a name still mentions it — a
    /// `RemoveLabels` reads exactly like an `AddLabels`, a property `REMOVE`
    /// like a `SET` — because a name outlives its last write. A drop names
    /// only its schema object (an index or constraint name), never the label
    /// or key it was built on, so it cannot un-name one either.
    ///
    /// The names are the graph catalog's. Two DDL families name them without
    /// writing data: an index or constraint create names its subject label
    /// (or relationship type) and its property keys, and the typed index
    /// detail names every indexed property plus a vector index's `WITH`
    /// keys. `CreateIndex.property` carries the legacy `{props}#{KIND}#{name}`
    /// encoding on the constraint-backed rows (and a bare name on the IR
    /// path), so its head is the key
    /// ([`split_legacy_property_encoding`]). Store metadata — index,
    /// constraint, database, graph-type and ontology names — is not a graph
    /// catalog name and contributes nothing.
    pub(crate) fn catalog_names(&self) -> Vec<CatalogName<'_>> {
        /// Append `names` as mentions built by `mention`.
        fn push<'a>(
            out: &mut Vec<CatalogName<'a>>,
            names: impl IntoIterator<Item = &'a String>,
            mention: fn(&'a str) -> CatalogName<'a>,
        ) {
            out.extend(names.into_iter().map(|name| mention(name)));
        }

        match self {
            Self::CreateNode {
                labels, properties, ..
            } => {
                let mut names = Vec::with_capacity(labels.len() + properties.len());
                push(&mut names, labels.iter(), CatalogName::Label);
                push(&mut names, properties.keys(), CatalogName::PropertyKey);
                names
            }
            Self::CreateRel {
                rel_type,
                properties,
                ..
            } => {
                let mut names = Vec::with_capacity(1 + properties.len());
                names.push(CatalogName::RelType(rel_type));
                push(&mut names, properties.keys(), CatalogName::PropertyKey);
                names
            }
            Self::SetNodeProp { key, .. }
            | Self::RemoveNodeProp { key, .. }
            | Self::SetRelProp { key, .. }
            | Self::RemoveRelProp { key, .. } => vec![CatalogName::PropertyKey(key)],
            Self::AddLabels { labels, .. } | Self::RemoveLabels { labels, .. } => {
                let mut names = Vec::with_capacity(labels.len());
                push(&mut names, labels.iter(), CatalogName::Label);
                names
            }
            Self::CreateIndex {
                label, property, ..
            } => vec![
                CatalogName::Label(label),
                CatalogName::PropertyKey(split_legacy_property_encoding(property).0),
            ],
            Self::CreateConstraint {
                relationship,
                label,
                properties,
                ..
            } => {
                let mut names = Vec::with_capacity(1 + properties.len());
                names.push(if *relationship {
                    CatalogName::RelType(label)
                } else {
                    CatalogName::Label(label)
                });
                push(&mut names, properties.iter(), CatalogName::PropertyKey);
                names
            }
            Self::CatalogIndexDetail { detail, .. } => {
                let mut names = Vec::with_capacity(
                    detail.labels.len() + detail.properties.len() + detail.with_properties.len(),
                );
                if detail.relationship {
                    push(&mut names, detail.labels.iter(), CatalogName::RelType);
                } else {
                    push(&mut names, detail.labels.iter(), CatalogName::Label);
                }
                push(
                    &mut names,
                    detail.properties.iter(),
                    CatalogName::PropertyKey,
                );
                push(
                    &mut names,
                    detail.with_properties.iter(),
                    CatalogName::PropertyKey,
                );
                names
            }
            Self::CatalogDerivedConstraint { constraint } => {
                // The op is only constructed from a `Constraint` whose
                // `entity_type` is `EntityKind::display_name()`, and replay
                // rejects any other spelling (the same classification
                // `graph_type::validate` applies to a stored constraint).
                let relationship =
                    cypherlite_model::EntityKind::from_display_name(&constraint.entity_type)
                        == Some(cypherlite_model::EntityKind::Relationship);
                let mut names = Vec::with_capacity(2 + constraint.properties.len());
                names.push(if relationship {
                    CatalogName::RelType(&constraint.label)
                } else {
                    CatalogName::Label(&constraint.label)
                });
                // An enforced label is always a node label, including on a
                // relationship constraint (`sourceLabel`/`targetLabel` name
                // the endpoint's label).
                if let Some(enforced) = &constraint.enforced_label {
                    names.push(CatalogName::Label(enforced));
                }
                push(
                    &mut names,
                    constraint.properties.iter(),
                    CatalogName::PropertyKey,
                );
                names
            }
            // A delete names no catalog name, and a drop names only its own
            // schema object — never the label or key it was built on.
            Self::DeleteNode { .. }
            | Self::DeleteRel { .. }
            | Self::DropIndex { .. }
            | Self::DropConstraint { .. } => Vec::new(),
            // Store metadata, not graph catalog names: database names, the
            // graph-type schema, a pull identity, an ontology document.
            Self::PutDatabase { .. }
            | Self::DropDatabase { .. }
            | Self::PutGraphType { .. }
            | Self::SetCurrentGraphType { .. }
            | Self::PullIdentityPut { .. }
            | Self::OntologySet { .. }
            | Self::OntologyDrop { .. } => Vec::new(),
        }
    }
}

/// What an op writes (ADR-0015 conflict classification). Total over
/// [`RedoOp`]: a delete is also a mutation of the same element,
/// `CreatedRel` carries the endpoints, and catalog ops carry their
/// (borrowed) identity — consumers match once and bind everything from the
/// classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteTarget<'a> {
    CreatedNode(NodeId),
    CreatedRel {
        id: RelId,
        start: NodeId,
        end: NodeId,
    },
    DeletedNode(NodeId),
    DeletedRel(RelId),
    MutatedNode(NodeId),
    MutatedRel(RelId),
    CreateIndex {
        name: &'a str,
        label: &'a str,
        property: &'a str,
    },
    DropIndex {
        name: &'a str,
    },
    CreateConstraint {
        name: &'a str,
    },
    DropConstraint {
        name: &'a str,
    },
    /// Store/catalog metadata mutation (index details, derived
    /// constraints, databases, graph type).
    CatalogMeta,
    /// Source-scoped pull identity (ADR-0080); not a graph CDC event.
    PullIdentity,
    /// Store-level ontology document (ADR-0120 §3): not a graph op and not
    /// catalog metadata, so it conflicts with nothing at the graph layer.
    Ontology,
}

impl WriteTarget<'_> {
    /// Whether the classified op changes the catalog (indexes, constraints,
    /// store metadata) rather than an entity or a pull identity. Total, so
    /// a new target must be placed on one side.
    pub(crate) fn is_catalog(&self) -> bool {
        match self {
            WriteTarget::CreatedNode(_)
            | WriteTarget::CreatedRel { .. }
            | WriteTarget::DeletedNode(_)
            | WriteTarget::DeletedRel(_)
            | WriteTarget::MutatedNode(_)
            | WriteTarget::MutatedRel(_)
            | WriteTarget::PullIdentity
            | WriteTarget::Ontology => false,
            WriteTarget::CreateIndex { .. }
            | WriteTarget::DropIndex { .. }
            | WriteTarget::CreateConstraint { .. }
            | WriteTarget::DropConstraint { .. }
            | WriteTarget::CatalogMeta => true,
        }
    }
}

/// One catalog name a [`RedoOp`] mentions ([`RedoOp::catalog_names`]), in
/// the namespace it belongs to. Borrowed from the op — the derivation never
/// copies a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogName<'a> {
    Label(&'a str),
    RelType(&'a str),
    PropertyKey(&'a str),
}

/// Node vs relationship for [`RedoOp::PullIdentityPut`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PullKind {
    Node,
    Rel,
}

#[cfg(test)]
mod tests;
