//! `SHOW INDEXES` row producer and its rendering helpers.

use std::collections::BTreeMap;

use cypherlite_core::{Result, Value};
use cypherlite_model::EntityKind;

use crate::ctx::QueryCtx;
use crate::eval::Row;
use crate::overlay::ExecGraph;

use super::super::catalog::{backtick, catalog_row, string_list};
use super::super::QueryResult;
use super::entity_variable;
use crate::redo::{split_legacy_property_encoding, IndexDetailSpec, IndexKindSpec};

// `SHOW INDEXES` rendering helpers over the shared
// `cypherlite_model::EntityKind`: they bind the index pattern shapes to
// the entity decision instead of re-branching on a bare
// `relationship: bool` at each use site; the pattern variable itself is
// the shared `super::entity_variable` decision. The `entityType` column
// uses `EntityKind::display_name` directly.

/// The `FOR` pattern for a label/type-token index, quoting and
/// `|`-joining the labels itself: `(n:`A`|`B`)` / `()-[r:`T`]-()`.
fn entity_pattern(entity: EntityKind, labels: &[String]) -> String {
    let variable = entity_variable(entity);
    let tokens = labels
        .iter()
        .map(|label| backtick(label))
        .collect::<Vec<_>>()
        .join("|");
    match entity {
        EntityKind::Node => format!("({variable}:{tokens})"),
        EntityKind::Relationship => format!("()-[{variable}:{tokens}]-()"),
    }
}

/// The LOOKUP pattern and EACH expression:
/// `FOR (n) ON EACH labels(n)` / `FOR ()-[r]-() ON EACH type(r)`.
fn entity_lookup_pattern_each(entity: EntityKind) -> (String, String) {
    let variable = entity_variable(entity);
    match entity {
        EntityKind::Node => (format!("({variable})"), format!("labels({variable})")),
        EntityKind::Relationship => (format!("()-[{variable}]-()"), format!("type({variable})")),
    }
}

/// The `OPTIONS {indexConfig: {...}}` suffix of a createStatement.
/// `None` when the stored options cannot be faithfully rendered — a
/// present `indexConfig` that is not a map, or a non-scalar entry (a
/// Float, a nested map, …) — never an inlined fake `null` literal. No
/// `indexConfig` renders as the empty string.
fn render_index_options(stored_options: &BTreeMap<String, Value>) -> Option<String> {
    let config = match stored_options.get("indexConfig") {
        None => return Some(String::new()),
        Some(Value::Map(config)) => config,
        Some(_) => return None,
    };
    let mut entries = Vec::with_capacity(config.len());
    for (key, value) in config.iter() {
        let rendered = match value {
            Value::String(value) => format!("'{value}'"),
            Value::Int(value) => value.to_string(),
            Value::Bool(value) => value.to_string(),
            _ => return None,
        };
        entries.push(format!("{}: {rendered}", backtick(key)));
    }
    Some(format!(
        " OPTIONS {{indexConfig: {{{}}}}}",
        entries.join(",")
    ))
}

/// The `createStatement` column for one index row: Neo4j CE's rendered
/// CREATE statement, or `None` when it cannot be faithfully rendered
/// (Cypher 5 hides the multi-label / WITH-properties forms; an empty
/// property list or a non-scalar `indexConfig` entry has no faithful
/// rendering). The caller maps `None` to the column's NULL.
fn index_create_statement(
    detail: &IndexDetailSpec,
    name: &str,
    entity: EntityKind,
    stored_options: &BTreeMap<String, Value>,
    cypher5: bool,
) -> Option<String> {
    if cypher5 && (detail.labels.len() > 1 || !detail.with_properties.is_empty()) {
        return None;
    }
    let variable = entity_variable(entity);
    let name = backtick(name);
    // LOOKUP indexes carry no label/property tokens: render the EACH
    // form (Neo4j CE: `FOR (n) ON EACH labels(n)` /
    // `FOR ()-[r]-() ON EACH type(r)`) — the generic path below would
    // render the broken `FOR (n:) ON (n.``)`.
    if matches!(detail.kind, IndexKindSpec::Lookup) {
        let (pattern, each) = entity_lookup_pattern_each(entity);
        return Some(format!(
            "CREATE LOOKUP INDEX {name} FOR {pattern} ON EACH {each}"
        ));
    }
    // A property-less index has no faithful ON clause (`ON (n.``)`) —
    // fail closed rather than render a broken statement.
    if detail.properties.is_empty() {
        return None;
    }
    let pattern = entity_pattern(entity, &detail.labels);
    let prop_refs = |props: &[String]| {
        props
            .iter()
            .map(|property| format!("{variable}.{}", backtick(property)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let on_list = prop_refs(&detail.properties);
    let with_text = if detail.with_properties.is_empty() {
        String::new()
    } else {
        format!(" WITH [{}]", prop_refs(&detail.with_properties))
    };
    let options_text = render_index_options(stored_options)?;
    Some(format!(
        "CREATE {} INDEX {name} FOR {pattern} ON ({on_list}){with_text}{options_text}",
        detail.kind.display_name(),
    ))
}

/// One `SHOW INDEXES` row: fall back through the three label/property
/// sources (index detail, backing constraint, legacy `IndexDef`
/// encoding), and assemble the Neo4j CE column set. `backing` is the
/// row's index-backed constraint of the same name, resolved once by the
/// caller.
fn index_row(
    idx: cypherlite_catalog::IndexDef,
    detail: Option<&IndexDetailSpec>,
    kind: IndexKindSpec,
    backing: Option<&cypherlite_catalog::ConstraintDef>,
    index_options: &BTreeMap<String, BTreeMap<String, Value>>,
    cypher5: bool,
) -> Row {
    let entity = EntityKind::from_relationship_flag(
        detail.is_some_and(|d| d.relationship)
            || backing.is_some_and(|constraint| constraint.relationship),
    );
    let labels = detail
        .map(|d| d.labels.clone())
        .or_else(|| backing.map(|constraint| vec![constraint.label.clone()]))
        .unwrap_or_else(|| vec![idx.label.clone()]);
    let properties = detail
        .map(|d| {
            let mut p = d.properties.clone();
            p.extend(d.with_properties.clone());
            p
        })
        .or_else(|| backing.map(|constraint| constraint.properties.clone()))
        .unwrap_or_else(|| {
            split_legacy_property_encoding(&idx.property)
                .0
                .split(',')
                .map(str::to_string)
                .collect()
        });
    let owning = backing
        .map(|def| Value::String(def.name.as_str().into()))
        .unwrap_or(Value::Null);
    // One decision: LOOKUP indexes have no label/property columns.
    let (labels_value, properties_value) = if matches!(kind, IndexKindSpec::Lookup) {
        (Value::Null, Value::Null)
    } else {
        (string_list(labels), string_list(properties))
    };
    let stored_options = index_options.get(&idx.name).cloned().unwrap_or_default();
    let create_statement = detail
        .and_then(|d| index_create_statement(d, &idx.name, entity, &stored_options, cypher5))
        .map(|s| Value::String(s.into()))
        .unwrap_or(Value::Null);
    catalog_row([
        ("id", Value::Int(0)),
        ("name", Value::String(idx.name.as_str().into())),
        ("state", Value::String("ONLINE".into())),
        ("populationPercent", Value::Float(100.0)),
        ("type", Value::String(kind.display_name().into())),
        ("entityType", Value::String(entity.display_name().into())),
        ("labelsOrTypes", labels_value),
        ("properties", properties_value),
        ("indexProvider", Value::String(kind.provider_name().into())),
        ("owningConstraint", owning),
        ("lastRead", Value::Null),
        ("readCount", Value::Int(0)),
        (
            "trackedSince",
            Value::Temporal(cypherlite_core::Temporal::DateTime {
                utc: chrono::Utc::now().naive_utc() - chrono::Duration::minutes(1),
                offset: 0,
                zone: None,
            }),
        ),
        ("options", Value::Map(stored_options.into())),
        ("failureMessage", Value::String("".into())),
        ("createStatement", create_statement),
    ])
}

/// The planner-IR `Clause::ShowIndexes` result (a fixed four-column
/// name/label/property/type table over the named indexes), kept beside the
/// parser-clause `SHOW INDEXES` row producer above.
pub(crate) fn show_indexes_result(graph: &ExecGraph<'_>) -> QueryResult {
    let mut defs = graph.named_indexes();
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    let rows = defs
        .into_iter()
        .map(|d| {
            vec![
                Value::String(d.name.into()),
                Value::String(d.label.into()),
                Value::String(d.property.into()),
                Value::String("RANGE".into()),
            ]
        })
        .collect();
    QueryResult::intermediate(
        vec![
            "name".into(),
            "label".into(),
            "property".into(),
            "type".into(),
        ],
        rows,
        graph.stats(),
    )
}

pub(crate) fn show_indexes_rows(
    graph: &ExecGraph<'_>,
    c: &cypherlite_parser::ShowIndexesClause,
    qctx: QueryCtx<'_>,
) -> Result<Vec<Row>> {
    let show = graph.show_schema()?;
    let details = &show.data.index_details;
    let index_options = &show.data.index_options;
    let constraints = graph.named_constraints();
    // Index-backed constraints by name, resolved once up front: each
    // row's backing lookup is a map hit instead of a re-scan of every
    // constraint with an inline kind parse.
    let backing_by_name: BTreeMap<&str, &cypherlite_catalog::ConstraintDef> = constraints
        .iter()
        .filter(|def| def.is_index_backed())
        .map(|def| (def.name.as_str(), def))
        .collect();
    Ok(graph
        .named_indexes()
        .into_iter()
        .filter_map(|idx| {
            // Resolve the detail and the typed kind once per row; the
            // kind filter, the row columns, and the createStatement all
            // read from this single binding. A detail join miss is a
            // legacy WAL/checkpoint row that predates CatalogIndexDetail
            // (catalog_create_index writes both redo ops in one statement
            // today); recover its kind from the `{props}#{KIND}#{name}`
            // encoding rather than defaulting to RANGE.
            let detail = details.get(&idx.name);
            let kind = detail
                .map(|d| d.kind)
                .or_else(|| split_legacy_property_encoding(&idx.property).1)
                .unwrap_or(IndexKindSpec::Range);
            let passes_filter = c
                .kind_filter
                .is_none_or(|wanted| IndexKindSpec::from(wanted) == kind);
            passes_filter.then_some((idx, detail, kind))
        })
        .map(|(idx, detail, kind)| {
            let backing = backing_by_name.get(idx.name.as_str()).copied();
            index_row(idx, detail, kind, backing, index_options, qctx.is_cypher5())
        })
        .collect())
}
