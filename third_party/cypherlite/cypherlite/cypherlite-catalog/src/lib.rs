use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::str::FromStr;

use cypherlite_core::{Error, Result};
use serde::{Deserialize, Serialize};

/// Named `(label, property)` catalog entry (ADR-0040).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDef {
    pub name: String,
    pub label: String,
    pub property: String,
}

impl IndexDef {
    /// The index's `:Label(property)` schema text — the one spelling the
    /// schema-naming reads render (the plan tree's `AllNodesScan`
    /// suggestion, the warnings that name an existing index, the store
    /// manifest's index lines), never a per-site `format!`. The property renders its bare name:
    /// `IndexDef.property` is dual-encoded in the catalog (bare for
    /// `CREATE INDEX` rows, legacy `{property}#{KIND}#{name}` for
    /// constraint-backed index rows), and this text names the schema,
    /// not the encoding — the head of the encoding, which is the whole
    /// property when it is bare. The executor's `redo` module owns the
    /// full typed decoder (`split_legacy_property_encoding`); this is
    /// its head half, the one fact the catalog's own render reads: the
    /// schema text never shows the legacy storage encoding.
    pub fn index_text(&self) -> String {
        let property = self
            .property
            .split('#')
            .next()
            .expect("a split yields a head");
        format!(":{}({})", self.label, property)
    }
}

/// Named uniqueness / existence constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConstraintDef {
    pub name: String,
    #[serde(default)]
    pub relationship: bool,
    pub label: String,
    pub properties: Vec<String>,
    pub kind: String,
    #[serde(default)]
    pub property_type: Option<String>,
    /// The label a `labelExistence`/`sourceLabel`/`targetLabel` constraint
    /// requires to be present (CIP-177, CIP-266) — part of the constraint's
    /// schema identity, the piece the theater's derived-constraint index
    /// used to hold alone. `None` for every other kind. Rows written before
    /// this field existed decode as `None` (their identity is unknown; the
    /// catalog treats them with the pre-field kind-scoped equivalence
    /// policy). `skip_serializing_if` keeps a non-enforced row's bytes
    /// identical to the pre-field shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforced_label: Option<String>,
}

/// Whether a constraint kind tag is backed by a range index — the single
/// `&str`-boundary predicate for the stringly-typed kind: the tag is
/// parsed via [`cypherlite_model::ConstraintType`] and mapped to
/// [`cypherlite_model::ConstraintType::is_index_backed`]. An unparseable
/// tag is not index-backed (fail closed). The free-function form of
/// [`ConstraintDef::is_index_backed`], for callers holding a bare kind
/// tag rather than a stored definition.
pub fn constraint_kind_is_index_backed(kind: &str) -> bool {
    cypherlite_model::ConstraintType::from_str(kind).is_ok_and(|kind| kind.is_index_backed())
}

impl ConstraintDef {
    /// Whether this constraint's kind is backed by a range index —
    /// delegates to [`constraint_kind_is_index_backed`], the single
    /// `&str`-boundary predicate for the stringly-typed `kind`.
    pub fn is_index_backed(&self) -> bool {
        constraint_kind_is_index_backed(&self.kind)
    }

    /// The catalog definition a derived model constraint describes — the
    /// one model→def crossing (its inverse is [`to_model`](Self::to_model)).
    /// The caller passes the typed entity fact, so the relationship bit is
    /// derived once here.
    pub fn from_model(
        constraint: &cypherlite_model::Constraint,
        entity: cypherlite_model::EntityKind,
    ) -> Self {
        Self {
            name: constraint.name.clone(),
            relationship: entity.is_relationship(),
            label: constraint.label.clone(),
            properties: constraint.properties.clone(),
            kind: constraint.constraint_type.as_str().to_string(),
            property_type: constraint.property_type.clone(),
            enforced_label: constraint.enforced_label.clone(),
        }
    }

    /// The model record this definition describes — the def→model crossing,
    /// taking the already-parsed kind. The definition owns its entity side,
    /// so the record derives it here (one input per fact). The theater's
    /// derived-constraint index uses it, so the statement keeps one
    /// constraint shape (the def) and does not respell its fields.
    pub fn to_model(
        &self,
        constraint_type: cypherlite_model::ConstraintType,
    ) -> cypherlite_model::Constraint {
        cypherlite_model::Constraint {
            name: self.name.clone(),
            label: self.label.clone(),
            properties: self.properties.clone(),
            constraint_type,
            property_type: self.property_type.clone(),
            entity_type: cypherlite_model::EntityKind::from_relationship_flag(self.relationship)
                .display_name()
                .into(),
            enforced_label: self.enforced_label.clone(),
        }
    }

    /// Whether `other` names the same subject and kind with different
    /// options: the same entity side, label, property set and kind, but a
    /// different property type (for the kinds that carry one). Distinct from
    /// [`schema_equivalent`](Self::schema_equivalent), which also matches the
    /// options. Two enforced-label rules with different labels are distinct
    /// rules, not conflicts (CIP-266), so a differing enforced label takes
    /// the pair out of this predicate.
    pub fn conflicts_with(&self, other: &ConstraintDef) -> bool {
        self.relationship == other.relationship
            && self.label == other.label
            && self.properties == other.properties
            && self.kind == other.kind
            && self.enforced_label == other.enforced_label
            && self.property_type != other.property_type
    }

    /// The constraint's schema text — `kind node|relationship :Label(props)
    /// [:: TYPE] [=> :EnforcedLabel]` — the one spelling the store
    /// manifest's constraint line renders (the [`IndexDef::index_text`]
    /// twin). Presentation sanitization stays with the caller.
    pub fn schema_text(&self) -> String {
        let target = if self.relationship {
            "relationship"
        } else {
            "node"
        };
        let mut text = format!(
            "{} {} :{}({})",
            self.kind,
            target,
            self.label,
            self.properties.join(", ")
        );
        if let Some(property_type) = &self.property_type {
            text.push_str(" :: ");
            text.push_str(property_type);
        }
        if let Some(enforced_label) = &self.enforced_label {
            text.push_str(" => :");
            text.push_str(enforced_label);
        }
        text
    }

    /// The constraint-equivalence predicate: same entity kind
    /// (node/relationship), label, property set, constraint kind, property
    /// type, and enforced label. The one place this six-field rule lives;
    /// the constraint's own name is not part of it.
    pub fn schema_equivalent(&self, other: &ConstraintDef) -> bool {
        self.relationship == other.relationship
            && self.label == other.label
            && self.properties == other.properties
            && self.kind == other.kind
            && self.property_type == other.property_type
            && self.enforced_label == other.enforced_label
    }
}

/// In-memory catalog of labels, relationship types, and named indexes.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub labels: BTreeSet<String>,
    pub rel_types: BTreeSet<String>,
    pub indexes: BTreeMap<String, IndexDef>,
    pub constraints: BTreeMap<String, ConstraintDef>,
}

impl Catalog {
    pub fn record_labels(&mut self, labels: impl IntoIterator<Item = String>) {
        self.labels.extend(labels);
    }

    pub fn record_rel_type(&mut self, t: impl Into<String>) {
        self.rel_types.insert(t.into());
    }

    /// Record one DDL-named graph catalog name in the namespace its entity
    /// kind selects (ADR-0363 decision 5): `relationship` names a
    /// relationship type, otherwise a node label. The one entity-kind split
    /// every schema-object insertion and redo replay shares.
    pub fn record_schema_name(&mut self, relationship: bool, name: String) {
        if relationship {
            self.record_rel_type(name);
        } else {
            self.record_labels([name]);
        }
    }

    pub fn index_by_schema(&self, label: &str, property: &str) -> Option<&IndexDef> {
        self.indexes
            .values()
            .find(|i| i.label == label && i.property == property)
    }

    pub fn insert_index(&mut self, def: IndexDef) -> Result<()> {
        // A named index's subject is a graph-catalog fact (ADR-0363
        // decision 5): a replaying insert records it, and the catalog's name
        // sets are monotone — a later `DROP INDEX` keeps the name, like a
        // label outlives its last node. `IndexDef` carries no entity kind, so
        // its subject is recorded as a node label here; the typed
        // `CatalogIndexDetail` additionally records a relationship index's
        // type in the relationship-type set. A rejected insert records
        // nothing.
        if let Some(existing) = self.indexes.get(&def.name) {
            if existing.label == def.label && existing.property == def.property {
                self.record_schema_name(false, def.label.clone());
                return Ok(());
            }
            return Err(Error::semantic(format!(
                "IndexAlreadyExists: `{}` already exists",
                def.name
            )));
        }
        if let Some(other) = self.index_by_schema(&def.label, &def.property) {
            return Err(Error::semantic(format!(
                "EquivalentSchemaRuleAlreadyExists: (:{} {{{}}}) already indexed as `{}`",
                def.label, def.property, other.name
            )));
        }
        self.record_schema_name(false, def.label.clone());
        self.indexes.insert(def.name.clone(), def);
        Ok(())
    }

    pub fn drop_index(&mut self, name: &str) -> bool {
        self.indexes.remove(name).is_some()
    }

    pub fn insert_constraint(&mut self, def: ConstraintDef) -> Result<()> {
        // The kind vocabulary is closed: every executor-authored definition
        // spells its kind with `ConstraintType::as_str()`. An unknown tag is
        // a corrupt durable row — reject it here rather than install a
        // constraint no validator, index, or committed tuple map can
        // maintain. The error text matches the validator's fail-closed
        // `ConstraintCheck::new` reply.
        let kind = match cypherlite_model::ConstraintType::from_str(&def.kind) {
            Ok(kind) => kind,
            Err(reason) => {
                return Err(Error::semantic(
                    cypherlite_model::ConstraintType::unknown_kind_message(&def.name, &reason),
                ));
            }
        };
        // A constraint's subject is a graph-catalog fact in the namespace its
        // entity kind selects (ADR-0363 decision 5), recorded by a replaying
        // identical insert too so an older store still learns it. Monotone: a
        // later `DROP CONSTRAINT` keeps the name. A rejected insert records
        // nothing.
        if let Some(existing) = self.constraints.get(&def.name) {
            if existing == &def {
                self.record_schema_name(def.relationship, def.label.clone());
                return Ok(());
            }
            return Err(Error::semantic(format!(
                "ConstraintAlreadyExists: `{}` already exists",
                def.name
            )));
        }
        // The identity is the six-field schema rule. A row written before
        // `enforced_label` existed decodes it as `None`; for an
        // enforced-label kind that leaves the identity unknown, so such a
        // row takes the pre-field kind-scoped policy (never declared
        // equivalent) — a legacy multi-parent graph type's two same-schema
        // existence constraints still load. Two current rows with different
        // enforced labels are two rules, not a duplicate (CIP-266's
        // multi-parent `(:A => :B&C)` derives two).
        let legacy_identity = kind.is_enforced_label_kind() && def.enforced_label.is_none();
        if !legacy_identity
            && self
                .constraints
                .values()
                .any(|existing| existing.schema_equivalent(&def))
        {
            return Err(Error::semantic(
                "EquivalentSchemaRuleAlreadyExists: equivalent constraint exists",
            ));
        }
        // Same subject and kind with different options (a property-type
        // constraint under another declared type) is a conflict, not a
        // distinct rule.
        if self
            .constraints
            .values()
            .any(|existing| def.conflicts_with(existing))
        {
            return Err(Error::semantic("ConstraintConflict"));
        }
        self.record_schema_name(def.relationship, def.label.clone());
        self.constraints.insert(def.name.clone(), def);
        Ok(())
    }

    pub fn drop_constraint(&mut self, name: &str) -> bool {
        self.constraints.remove(name).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical constraint fixture: one name/label/property, with the
    /// kind parameterized.
    fn def_of(kind: &str) -> ConstraintDef {
        ConstraintDef {
            name: "c".into(),
            relationship: false,
            label: "L".into(),
            properties: vec!["p".into()],
            kind: kind.into(),
            property_type: None,
            enforced_label: None,
        }
    }

    #[test]
    fn records() {
        let mut c = Catalog::default();
        c.record_labels(["Person".into()]);
        c.record_rel_type("KNOWS");
        assert!(c.labels.contains("Person"));
        assert!(c.rel_types.contains("KNOWS"));
    }

    #[test]
    fn constraint_def_is_index_backed_parses_kind_at_str_boundary() {
        assert!(def_of("unique").is_index_backed());
        assert!(def_of("key").is_index_backed());
        assert!(!def_of("exists").is_index_backed());
        assert!(!def_of("propertyType").is_index_backed());
        assert!(!def_of("labelExistence").is_index_backed());
        assert!(!def_of("nodeLabelExistence").is_index_backed());
        assert!(!def_of("sourceLabel").is_index_backed());
        assert!(!def_of("sourceLabelExistence").is_index_backed());
        assert!(!def_of("targetLabel").is_index_backed());
        assert!(!def_of("targetLabelExistence").is_index_backed());
        // An unparseable tag fails closed: not index-backed.
        assert!(!def_of("not-a-kind").is_index_backed());
        assert!(!def_of("").is_index_backed());
    }

    #[test]
    fn constraint_def_is_index_backed_delegates_to_the_free_fn() {
        for kind in [
            "unique",
            "key",
            "exists",
            "propertyType",
            "labelExistence",
            "nodeLabelExistence",
            "sourceLabel",
            "sourceLabelExistence",
            "targetLabel",
            "targetLabelExistence",
            "not-a-kind",
            "",
        ] {
            assert_eq!(
                def_of(kind).is_index_backed(),
                constraint_kind_is_index_backed(kind),
                "{kind}"
            );
        }
    }

    #[test]
    fn constraint_def_schema_equivalent_compares_all_six_fields() {
        let def = ConstraintDef {
            name: "c".into(),
            relationship: false,
            label: "L".into(),
            properties: vec!["p".into()],
            kind: "propertyType".into(),
            property_type: Some("STRING".into()),
            enforced_label: None,
        };
        let props = vec!["p".to_string()];
        let probe = |relationship: bool,
                     label: &str,
                     props: &[String],
                     kind: &str,
                     ty: Option<&str>,
                     enforced: Option<&str>| ConstraintDef {
            name: "probe".into(),
            relationship,
            label: label.to_string(),
            properties: props.to_vec(),
            kind: kind.to_string(),
            property_type: ty.map(str::to_owned),
            enforced_label: enforced.map(str::to_owned),
        };
        assert!(def.schema_equivalent(&probe(
            false,
            "L",
            &props,
            "propertyType",
            Some("STRING"),
            None
        )));
        // The constraint's own name is not part of the equivalence rule.
        assert!(ConstraintDef {
            name: "other".into(),
            ..def.clone()
        }
        .schema_equivalent(&probe(
            false,
            "L",
            &props,
            "propertyType",
            Some("STRING"),
            None
        )));
        // Flip each of the six fields in turn: no longer equivalent.
        assert!(!def.schema_equivalent(&probe(
            true,
            "L",
            &props,
            "propertyType",
            Some("STRING"),
            None
        )));
        assert!(!def.schema_equivalent(&probe(
            false,
            "M",
            &props,
            "propertyType",
            Some("STRING"),
            None
        )));
        assert!(!def.schema_equivalent(&probe(
            false,
            "L",
            &["q".to_string()],
            "propertyType",
            Some("STRING"),
            None
        )));
        assert!(!def.schema_equivalent(&probe(false, "L", &props, "unique", Some("STRING"), None)));
        assert!(!def.schema_equivalent(&probe(false, "L", &props, "propertyType", None, None)));
        assert!(!def.schema_equivalent(&probe(
            false,
            "L",
            &props,
            "propertyType",
            Some("INTEGER"),
            None
        )));
        assert!(!def.schema_equivalent(&probe(
            false,
            "L",
            &props,
            "propertyType",
            Some("STRING"),
            Some("Required")
        )));
        // An enforced-label identity is part of the rule: two existence
        // constraints with different enforced labels differ.
        let existence = |enforced: Option<&str>| ConstraintDef {
            name: "e".into(),
            relationship: false,
            label: "L".into(),
            properties: vec![],
            kind: "nodeLabelExistence".into(),
            property_type: None,
            enforced_label: enforced.map(str::to_owned),
        };
        assert!(existence(Some("A")).schema_equivalent(&existence(Some("A"))));
        assert!(!existence(Some("A")).schema_equivalent(&existence(Some("B"))));
        assert!(!existence(Some("A")).schema_equivalent(&existence(None)));
    }

    #[test]
    fn named_index_unique_name_and_schema() {
        let mut c = Catalog::default();
        c.insert_index(IndexDef {
            name: "person_name".into(),
            label: "Person".into(),
            property: "name".into(),
        })
        .unwrap();
        c.insert_index(IndexDef {
            name: "person_name".into(),
            label: "Person".into(),
            property: "name".into(),
        })
        .unwrap();
        assert!(c
            .insert_index(IndexDef {
                name: "person_name".into(),
                label: "Person".into(),
                property: "age".into(),
            })
            .is_err());
        assert!(c
            .insert_index(IndexDef {
                name: "other".into(),
                label: "Person".into(),
                property: "name".into(),
            })
            .is_err());
        assert!(c.drop_index("person_name"));
        assert!(!c.drop_index("person_name"));
    }

    /// The kind vocabulary is closed: an unknown tag is rejected loudly
    /// (ADR-0439) instead of installing a constraint no validator, index, or
    /// committed tuple map can maintain.
    #[test]
    fn insert_rejects_an_unknown_constraint_kind() {
        let mut c = Catalog::default();
        let def = ConstraintDef {
            name: "c".into(),
            relationship: false,
            label: "L".into(),
            properties: vec!["p".into()],
            kind: "uniqueish".into(),
            property_type: None,
            enforced_label: None,
        };
        let error = c.insert_constraint(def).unwrap_err();
        assert_eq!(
            error.to_string(),
            "semantic error: ConstraintValidationFailed: `c` unknown constraint type \"uniqueish\""
        );
        assert!(c.constraints.is_empty());
    }

    /// ADR-0363 decision 5: inserting a named schema object records its
    /// subject in the graph-catalog namespace the object's model selects — a
    /// node constraint's label or a relationship constraint's type — and a
    /// rejected insert records nothing.
    #[test]
    fn insert_records_schema_subject_names() {
        let mut c = Catalog::default();
        let constraint = |name: &str, relationship: bool, label: &str| ConstraintDef {
            name: name.into(),
            relationship,
            label: label.into(),
            properties: vec!["p".into()],
            kind: "unique".into(),
            property_type: None,
            enforced_label: None,
        };

        c.insert_index(IndexDef {
            name: "i".into(),
            label: "Indexed".into(),
            property: "k".into(),
        })
        .unwrap();
        c.insert_constraint(constraint("nc", false, "NodeC"))
            .unwrap();
        c.insert_constraint(constraint("rc", true, "RelC")).unwrap();

        assert!(c.labels.contains("Indexed"));
        assert!(c.labels.contains("NodeC"));
        assert!(c.rel_types.contains("RelC"));
        assert!(!c.labels.contains("RelC"));

        c.insert_index(IndexDef {
            name: "i".into(),
            label: "Other".into(),
            property: "k".into(),
        })
        .unwrap_err();
        c.insert_constraint(constraint("nc", false, "Other"))
            .unwrap_err();
        assert!(!c.labels.contains("Other"));
    }

    /// A same-subject, same-kind rule under a different property type is a
    /// conflict, not a distinct rule; the same type under a new name is the
    /// equivalent-schema reply.
    #[test]
    fn conflicting_property_types_are_a_conflict_not_a_distinct_rule() {
        let mut c = Catalog::default();
        let typed = |name: &str, ty: &str| ConstraintDef {
            name: name.into(),
            relationship: false,
            label: "L".into(),
            properties: vec!["p".into()],
            kind: "propertyType".into(),
            property_type: Some(ty.into()),
            enforced_label: None,
        };
        c.insert_constraint(typed("a", "INTEGER")).unwrap();
        let error = c.insert_constraint(typed("b", "STRING")).unwrap_err();
        assert_eq!(error.to_string(), "semantic error: ConstraintConflict");
        let error = c.insert_constraint(typed("c", "INTEGER")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "semantic error: EquivalentSchemaRuleAlreadyExists: equivalent constraint exists"
        );
    }

    /// CIP-266: a label-existence rule's identity includes its enforced
    /// label, so a multi-parent element type's two rules on one label
    /// coexist, while a repeat of the same rule does not.
    #[test]
    fn enforced_label_joins_the_constraint_identity() {
        let mut c = Catalog::default();
        let rule = |name: &str, enforced: &str| ConstraintDef {
            name: name.into(),
            relationship: false,
            label: "P".into(),
            properties: vec![],
            kind: "nodeLabelExistence".into(),
            property_type: None,
            enforced_label: Some(enforced.into()),
        };
        c.insert_constraint(rule("q", "Q")).unwrap();
        c.insert_constraint(rule("r", "R")).unwrap();
        let error = c.insert_constraint(rule("q_again", "Q")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "semantic error: EquivalentSchemaRuleAlreadyExists: equivalent constraint exists"
        );
    }

    /// A row written before `ConstraintDef` carried the enforced label
    /// decodes it as `None`; its identity is unknown, so the pre-field
    /// kind-scoped policy applies and legacy rows never block each other or
    /// a current row. This keeps a legacy multi-parent graph type loadable.
    #[test]
    fn legacy_rows_without_the_enforced_label_stay_kind_scoped() {
        let mut c = Catalog::default();
        let legacy = |name: &str| ConstraintDef {
            name: name.into(),
            relationship: false,
            label: "P".into(),
            properties: vec![],
            kind: "nodeLabelExistence".into(),
            property_type: None,
            enforced_label: None,
        };
        c.insert_constraint(legacy("a")).unwrap();
        c.insert_constraint(legacy("b")).unwrap();
        c.insert_constraint(ConstraintDef {
            name: "current".into(),
            enforced_label: Some("Q".into()),
            ..legacy("probe")
        })
        .unwrap();
    }
}
