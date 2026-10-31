//! Schema constraint types shared by CypherLite and (historically) GraphStore.
use std::fmt;
use std::str::FromStr;

/// The kind of a schema [`Constraint`], persisted as
/// `Constraint::constraint_type`.
///
/// Replaces the previous stringly-typed `constraint_type: String`. The serde
/// representation is the exact tag written by earlier versions, so pre-enum
/// databases load unchanged, and an unknown tag is a hard deserialization
/// error instead of being silently ignored by a `_ => {}` catch-all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ConstraintType {
    #[serde(rename = "unique")]
    Unique,
    #[serde(rename = "key")]
    Key,
    #[serde(rename = "exists")]
    Exists,
    #[serde(rename = "propertyType")]
    PropertyType,
    /// A node label-existence constraint derived from a graph type
    /// (`ALTER CURRENT GRAPH TYPE`).
    #[serde(rename = "labelExistence")]
    LabelExistence,
    /// The standalone `REQUIRE alias:Label` node label-existence form.
    #[serde(rename = "nodeLabelExistence")]
    NodeLabelExistence,
    /// A relationship source-label constraint derived from a graph type.
    #[serde(rename = "sourceLabel")]
    SourceLabel,
    /// The standalone relationship source-label form.
    #[serde(rename = "sourceLabelExistence")]
    SourceLabelExistence,
    /// A relationship target-label constraint derived from a graph type.
    #[serde(rename = "targetLabel")]
    TargetLabel,
    /// The standalone relationship target-label form.
    #[serde(rename = "targetLabelExistence")]
    TargetLabelExistence,
}

impl ConstraintType {
    /// The canonical stored tag — identical to the serde representation and to
    /// the pre-enum `Constraint::constraint_type` string.
    pub fn as_str(&self) -> &'static str {
        match self {
            ConstraintType::Unique => "unique",
            ConstraintType::Key => "key",
            ConstraintType::Exists => "exists",
            ConstraintType::PropertyType => "propertyType",
            ConstraintType::LabelExistence => "labelExistence",
            ConstraintType::NodeLabelExistence => "nodeLabelExistence",
            ConstraintType::SourceLabel => "sourceLabel",
            ConstraintType::SourceLabelExistence => "sourceLabelExistence",
            ConstraintType::TargetLabel => "targetLabel",
            ConstraintType::TargetLabelExistence => "targetLabelExistence",
        }
    }

    /// Whether this constraint kind is backed by a range index — the two kinds
    /// (`unique`/`key`) that share one physical backing index per schema.
    pub fn is_index_backed(&self) -> bool {
        matches!(self, ConstraintType::Unique | ConstraintType::Key)
    }

    /// Whether this kind is one of the six label/endpoint-label existence kinds
    /// that carry an enforced label (CIP-187): `labelExistence`,
    /// `nodeLabelExistence`, `sourceLabel`, `sourceLabelExistence`,
    /// `targetLabel`, `targetLabelExistence`. Single source of truth for the
    /// string check in the executor's `requires_enforced_label`.
    pub fn is_enforced_label_kind(&self) -> bool {
        self.enforced_label_site().is_some()
    }

    /// Where an enforced-label kind checks its enforced label: on the
    /// constrained node, or on the start or end node of the constrained
    /// relationship. `None` for every kind that carries no enforced label.
    /// This is the typed form of the enforced-label entity split: the kind
    /// attaches to a relationship iff the site is not
    /// [`EnforcedLabelSite::Node`].
    pub fn enforced_label_site(&self) -> Option<EnforcedLabelSite> {
        match self {
            ConstraintType::LabelExistence | ConstraintType::NodeLabelExistence => {
                Some(EnforcedLabelSite::Node)
            }
            ConstraintType::SourceLabel | ConstraintType::SourceLabelExistence => {
                Some(EnforcedLabelSite::Start)
            }
            ConstraintType::TargetLabel | ConstraintType::TargetLabelExistence => {
                Some(EnforcedLabelSite::End)
            }
            ConstraintType::Unique
            | ConstraintType::Key
            | ConstraintType::Exists
            | ConstraintType::PropertyType => None,
        }
    }

    /// Whether this kind enforces uniqueness of the constrained properties:
    /// `unique` and `key` (a key constraint is uniqueness + existence).
    /// Today this is exactly the index-backed set, but the predicate is
    /// named for the semantic guarantee, not the physical backing.
    pub fn enforces_uniqueness(&self) -> bool {
        matches!(self, ConstraintType::Unique | ConstraintType::Key)
    }

    /// Whether this kind requires the constrained properties to be present
    /// (non-null) on every matching entity: `exists`, and `key` (a key
    /// property must exist). The enforced-label kinds
    /// ([`ConstraintType::is_enforced_label_kind`]) enforce label presence,
    /// not property presence, so they are excluded.
    pub fn enforces_property_presence(&self) -> bool {
        matches!(self, ConstraintType::Exists | ConstraintType::Key)
    }
}

impl fmt::Display for ConstraintType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ConstraintType {
    type Err = String;
    /// Parse a stored tag. Unknown tags are rejected rather than silently
    /// defaulting.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "unique" => Ok(ConstraintType::Unique),
            "key" => Ok(ConstraintType::Key),
            "exists" => Ok(ConstraintType::Exists),
            "propertyType" => Ok(ConstraintType::PropertyType),
            "labelExistence" => Ok(ConstraintType::LabelExistence),
            "nodeLabelExistence" => Ok(ConstraintType::NodeLabelExistence),
            "sourceLabel" => Ok(ConstraintType::SourceLabel),
            "sourceLabelExistence" => Ok(ConstraintType::SourceLabelExistence),
            "targetLabel" => Ok(ConstraintType::TargetLabel),
            "targetLabelExistence" => Ok(ConstraintType::TargetLabelExistence),
            other => Err(format!("unknown constraint type {other:?}")),
        }
    }
}

impl PartialEq<str> for ConstraintType {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for ConstraintType {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// Which node an enforced-label constraint kind requires to carry its
/// enforced label ([`ConstraintType::enforced_label_site`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnforcedLabelSite {
    /// The constrained node itself (`labelExistence`, `nodeLabelExistence`).
    Node,
    /// The start node of the constrained relationship (`sourceLabel`,
    /// `sourceLabelExistence`).
    Start,
    /// The end node of the constrained relationship (`targetLabel`,
    /// `targetLabelExistence`).
    End,
}

impl EnforcedLabelSite {
    /// Whether the enforced label is checked on an endpoint of the
    /// constrained relationship (`Start`/`End`) rather than on the
    /// constrained node itself — the named form of the "the kind attaches
    /// to a relationship iff the site is not [`EnforcedLabelSite::Node`]"
    /// polarity rule ([`ConstraintType::enforced_label_site`]), so call
    /// sites compare against a `relationship` flag without hand-rolled
    /// boolean algebra.
    pub fn is_relationship(&self) -> bool {
        !matches!(self, EnforcedLabelSite::Node)
    }
}

fn default_node_entity_type() -> String {
    "NODE".to_string()
}

/// Node-vs-relationship entity kind: the typed form of the catalog's
/// `relationship: bool` flag and of the `"NODE"`/`"RELATIONSHIP"`
/// `entityType` string ([`Constraint::entity_type`], the `SHOW INDEXES` /
/// `SHOW CONSTRAINTS` `entityType` column, CIP-11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityKind {
    Node,
    Relationship,
}

impl EntityKind {
    /// The kind for a catalog `relationship` flag (`false` → node).
    pub fn from_relationship_flag(relationship: bool) -> Self {
        if relationship {
            EntityKind::Relationship
        } else {
            EntityKind::Node
        }
    }

    /// The catalog `relationship` flag for this kind — the inverse of
    /// [`EntityKind::from_relationship_flag`].
    pub fn is_relationship(&self) -> bool {
        matches!(self, EntityKind::Relationship)
    }

    /// The `entityType` display name (`"NODE"` / `"RELATIONSHIP"`).
    pub fn display_name(&self) -> &'static str {
        match self {
            EntityKind::Node => "NODE",
            EntityKind::Relationship => "RELATIONSHIP",
        }
    }

    /// Parse an `entityType` display name back into the kind — the inverse
    /// of [`EntityKind::display_name`]. Unknown names are rejected.
    pub fn from_display_name(name: &str) -> Option<Self> {
        match name {
            "NODE" => Some(EntityKind::Node),
            "RELATIONSHIP" => Some(EntityKind::Relationship),
            _ => None,
        }
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// CIP-186's three-way constraint classification: whether a constraint is
/// `undesignated` (a uniqueness/key constraint — never depends on the
/// current graph type), `dependent` (graph-type-derived, or an
/// existence/property-type constraint whose label identifies a node/edge
/// type), or `independent` (every other constraint). The typed form of the
/// strings `classify_constraint` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConstraintClassification {
    Undesignated,
    Dependent,
    Independent,
}

impl ConstraintClassification {
    /// The `SHOW CONSTRAINTS` `classification` column value.
    pub fn as_str(&self) -> &'static str {
        match self {
            ConstraintClassification::Undesignated => "undesignated",
            ConstraintClassification::Dependent => "dependent",
            ConstraintClassification::Independent => "independent",
        }
    }
}

impl fmt::Display for ConstraintClassification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A schema constraint (unique, exists, key, propertyType, or -- CIP-186/
/// CIP-187 -- one of the graph-type-derived kinds labelExistence/
/// sourceLabel/targetLabel).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Constraint {
    pub name: String,
    pub label: String,
    pub properties: Vec<String>,
    /// The constraint kind ([`ConstraintType`]). Serialized as the exact tag
    /// (`"unique"`, `"propertyType"`, ...).
    #[serde(rename = "type")]
    pub constraint_type: ConstraintType,
    /// The CIP-100-normalized type string for a `"propertyType"`
    /// constraint (CIP-85), `None` for every other `constraint_type`.
    /// `#[serde(default)]` keeps JSON databases written before this field
    /// existed loadable.
    #[serde(default)]
    pub property_type: Option<String>,
    /// "NODE" or "RELATIONSHIP" (CIP-11). `#[serde(default)]` keeps JSON
    /// databases written before this field existed loadable.
    #[serde(default = "default_node_entity_type")]
    pub entity_type: String,
    /// The label a `"labelExistence"`/`"sourceLabel"`/`"targetLabel"`
    /// graph-type-derived constraint (CIP-187) requires to be present --
    /// `SHOW CONSTRAINTS`'s `enforcedLabel` column. `None` for every other
    /// `constraint_type`. `#[serde(default)]` keeps JSON databases written
    /// before this field existed loadable.
    #[serde(default)]
    pub enforced_label: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_CONSTRAINT_TYPES: [ConstraintType; 10] = [
        ConstraintType::Unique,
        ConstraintType::Key,
        ConstraintType::Exists,
        ConstraintType::PropertyType,
        ConstraintType::LabelExistence,
        ConstraintType::NodeLabelExistence,
        ConstraintType::SourceLabel,
        ConstraintType::SourceLabelExistence,
        ConstraintType::TargetLabel,
        ConstraintType::TargetLabelExistence,
    ];

    #[test]
    fn enforces_uniqueness_is_unique_and_key_only() {
        for kind in ALL_CONSTRAINT_TYPES {
            assert_eq!(
                kind.enforces_uniqueness(),
                matches!(kind, ConstraintType::Unique | ConstraintType::Key),
                "{kind}"
            );
        }
    }

    #[test]
    fn enforces_property_presence_is_exists_and_key_only() {
        for kind in ALL_CONSTRAINT_TYPES {
            assert_eq!(
                kind.enforces_property_presence(),
                matches!(kind, ConstraintType::Exists | ConstraintType::Key),
                "{kind}"
            );
        }
    }

    #[test]
    fn enforced_label_site_follows_the_kind_tag() {
        for kind in ALL_CONSTRAINT_TYPES {
            let expected = match kind.as_str() {
                "labelExistence" | "nodeLabelExistence" => Some(EnforcedLabelSite::Node),
                "sourceLabel" | "sourceLabelExistence" => Some(EnforcedLabelSite::Start),
                "targetLabel" | "targetLabelExistence" => Some(EnforcedLabelSite::End),
                _ => None,
            };
            assert_eq!(kind.enforced_label_site(), expected, "{kind}");
            assert_eq!(kind.is_enforced_label_kind(), expected.is_some(), "{kind}");
        }
    }

    #[test]
    fn enforced_label_site_is_relationship_pins_the_polarity() {
        assert!(!EnforcedLabelSite::Node.is_relationship());
        assert!(EnforcedLabelSite::Start.is_relationship());
        assert!(EnforcedLabelSite::End.is_relationship());
    }

    #[test]
    fn constraint_classification_strings() {
        assert_eq!(
            ConstraintClassification::Undesignated.as_str(),
            "undesignated"
        );
        assert_eq!(ConstraintClassification::Dependent.as_str(), "dependent");
        assert_eq!(
            ConstraintClassification::Independent.as_str(),
            "independent"
        );
        assert_eq!(
            ConstraintClassification::Undesignated.to_string(),
            "undesignated"
        );
        assert_eq!(ConstraintClassification::Dependent.to_string(), "dependent");
        assert_eq!(
            ConstraintClassification::Independent.to_string(),
            "independent"
        );
    }

    #[test]
    fn entity_kind_relationship_flag_round_trips() {
        assert_eq!(EntityKind::from_relationship_flag(false), EntityKind::Node);
        assert_eq!(
            EntityKind::from_relationship_flag(true),
            EntityKind::Relationship
        );
        for kind in [EntityKind::Node, EntityKind::Relationship] {
            assert_eq!(
                EntityKind::from_relationship_flag(kind.is_relationship()),
                kind
            );
        }
    }

    #[test]
    fn entity_kind_display_name_round_trips() {
        assert_eq!(EntityKind::Node.display_name(), "NODE");
        assert_eq!(EntityKind::Relationship.display_name(), "RELATIONSHIP");
        assert_eq!(EntityKind::Node.to_string(), "NODE");
        for kind in [EntityKind::Node, EntityKind::Relationship] {
            assert_eq!(
                EntityKind::from_display_name(kind.display_name()),
                Some(kind)
            );
        }
        assert_eq!(EntityKind::from_display_name("node"), None);
        assert_eq!(EntityKind::from_display_name("EDGE"), None);
    }
}
