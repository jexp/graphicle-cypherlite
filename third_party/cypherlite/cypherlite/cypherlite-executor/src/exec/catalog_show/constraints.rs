//! `SHOW CONSTRAINTS` row producer and its rendering helpers.

use std::collections::BTreeMap;
use std::str::FromStr;

use cypherlite_core::{Result, Value};
use cypherlite_model::{ConstraintClassification, ConstraintType, EntityKind};

use crate::ctx::QueryCtx;
use crate::eval::Row;
use crate::overlay::ExecGraph;

use super::super::catalog::{catalog_row, string_list};
use super::entity_variable;

/// The `SHOW CONSTRAINTS` `type` column: Neo4j's underscore-style kind
/// name, entity-parameterized for the four property kinds. Deliberately
/// separate from `db.schema.visualization()`'s space-style kind names:
/// that mapping is entity-free (only node constraints ever render there)
/// and lives with the procedure rows, so a shared style-parameterized
/// helper would couple two surfaces that change for different reasons.
fn show_constraint_type_name(kind: ConstraintType, entity: EntityKind) -> &'static str {
    match kind {
        ConstraintType::Unique => match entity {
            EntityKind::Node => "NODE_PROPERTY_UNIQUENESS",
            EntityKind::Relationship => "RELATIONSHIP_PROPERTY_UNIQUENESS",
        },
        ConstraintType::Key => match entity {
            EntityKind::Node => "NODE_KEY",
            EntityKind::Relationship => "RELATIONSHIP_KEY",
        },
        ConstraintType::Exists => match entity {
            EntityKind::Node => "NODE_PROPERTY_EXISTENCE",
            EntityKind::Relationship => "RELATIONSHIP_PROPERTY_EXISTENCE",
        },
        ConstraintType::PropertyType => match entity {
            EntityKind::Node => "NODE_PROPERTY_TYPE",
            EntityKind::Relationship => "RELATIONSHIP_PROPERTY_TYPE",
        },
        ConstraintType::LabelExistence | ConstraintType::NodeLabelExistence => {
            "NODE_LABEL_EXISTENCE"
        }
        ConstraintType::SourceLabel => "RELATIONSHIP_SOURCE_LABEL",
        ConstraintType::SourceLabelExistence => "RELATIONSHIP_SOURCE_LABEL_EXISTENCE",
        ConstraintType::TargetLabel => "RELATIONSHIP_TARGET_LABEL",
        ConstraintType::TargetLabelExistence => "RELATIONSHIP_TARGET_LABEL_EXISTENCE",
    }
}

/// The `createStatement` column for one constraint row: Neo4j CE's
/// rendered CREATE CONSTRAINT statement, or `None` when there is no
/// faithful rendering — a dependent constraint (graph-type-managed; the
/// column is NULL), a label/endpoint-label or unrecognized kind (no
/// standalone CREATE form), or a Cypher 5 `propertyType` constraint (the
/// `IS :: <type>` verb is hidden). The caller maps `None` to the
/// column's NULL.
fn constraint_create_statement(
    def: &cypherlite_catalog::ConstraintDef,
    kind: Option<ConstraintType>,
    entity: EntityKind,
    classification: ConstraintClassification,
    cypher5: bool,
) -> Option<String> {
    if classification == ConstraintClassification::Dependent {
        return None;
    }
    let verb = match kind {
        Some(ConstraintType::Key) => Some("IS KEY".to_string()),
        Some(ConstraintType::Unique) => Some("IS UNIQUE".to_string()),
        Some(ConstraintType::Exists) => Some("IS NOT NULL".to_string()),
        Some(ConstraintType::PropertyType) if !cypher5 => {
            def.property_type.as_ref().map(|ty| format!("IS :: {ty}"))
        }
        _ => None,
    }?;
    let variable = entity_variable(entity);
    let pattern = match entity {
        EntityKind::Node => format!("({variable}:`{}`)", def.label),
        EntityKind::Relationship => format!("()-[{variable}:`{}`]-()", def.label),
    };
    let properties = def
        .properties
        .iter()
        .map(|property| format!("{variable}.`{property}`"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "CREATE CONSTRAINT `{}` FOR {pattern} REQUIRE ({properties}) {verb}",
        def.name
    ))
}

pub(crate) fn show_constraints_rows(
    graph: &ExecGraph<'_>,
    c: &cypherlite_parser::ShowConstraintsClause,
    qctx: QueryCtx<'_>,
) -> Result<Vec<Row>> {
    let show = graph.show_schema()?;
    let derived = &show.data.derived_constraints;
    let current = show.data.current_graph_type.to_body();
    Ok(graph
        .named_constraints()
        .into_iter()
        .filter_map(|def| {
            // Parse the stored kind tag and bind the entity kind once per
            // row; the filters, the `type` column, and the createStatement
            // all read from these typed bindings.
            let kind = ConstraintType::from_str(&def.kind).ok();
            let entity = EntityKind::from_relationship_flag(def.relationship);
            let entity_matches = c
                .type_filter
                .entity_str()
                .is_none_or(|name| EntityKind::from_display_name(name) == Some(entity));
            let kind_matches = c.type_filter.kind.is_none_or(|wanted| match wanted {
                cypherlite_parser::ConstraintKindFilter::Unique => {
                    kind == Some(ConstraintType::Unique)
                }
                cypherlite_parser::ConstraintKindFilter::Key => kind == Some(ConstraintType::Key),
                cypherlite_parser::ConstraintKindFilter::PropertyType => {
                    kind == Some(ConstraintType::PropertyType)
                }
                cypherlite_parser::ConstraintKindFilter::Existence => {
                    kind == Some(ConstraintType::Exists)
                        || (!c.type_filter.property_only
                            && matches!(
                                kind,
                                Some(
                                    ConstraintType::LabelExistence
                                        | ConstraintType::NodeLabelExistence
                                        | ConstraintType::SourceLabelExistence
                                        | ConstraintType::TargetLabelExistence
                                )
                            ))
                }
            });
            (entity_matches && kind_matches).then_some((def, kind, entity))
        })
        .map(|(def, kind, entity)| {
            let index_backed = kind.is_some_and(|kind| kind.is_index_backed());
            let detail = derived.get(&def.name);
            // `None` (an unparseable stored tag) keeps the legacy UNKNOWN
            // display; the typed mapping itself is total.
            let display_type =
                kind.map_or("UNKNOWN", |kind| show_constraint_type_name(kind, entity));
            let classification = detail
                .map(|constraint| crate::graph_type::classify_constraint(&current, constraint))
                .unwrap_or_else(|| {
                    crate::graph_type::classify_constraint_parts(&current, kind, entity, &def.label)
                });
            let create_statement =
                constraint_create_statement(&def, kind, entity, classification, qctx.is_cypher5())
                    .map(|s| Value::String(s.into()))
                    .unwrap_or(Value::Null);
            catalog_row([
                ("id", Value::Int(0)),
                ("name", Value::String(def.name.as_str().into())),
                ("type", Value::String(display_type.into())),
                ("entityType", Value::String(entity.display_name().into())),
                ("labelsOrTypes", string_list([def.label])),
                (
                    "properties",
                    if def.properties.is_empty() {
                        Value::Null
                    } else {
                        string_list(def.properties)
                    },
                ),
                (
                    "ownedIndex",
                    if index_backed {
                        Value::String(def.name.into())
                    } else {
                        Value::Null
                    },
                ),
                (
                    "propertyType",
                    def.property_type
                        .map(|s| Value::String(s.into()))
                        .unwrap_or(Value::Null),
                ),
                (
                    "enforcedLabel",
                    detail
                        .and_then(|constraint| constraint.enforced_label.as_deref())
                        .map(|s| Value::String(s.into()))
                        .unwrap_or(Value::Null),
                ),
                (
                    "classification",
                    Value::String(classification.as_str().into()),
                ),
                (
                    "options",
                    if detail.is_some() {
                        Value::Null
                    } else {
                        Value::Map(BTreeMap::new().into())
                    },
                ),
                ("createStatement", create_statement),
            ])
        })
        .collect())
}
