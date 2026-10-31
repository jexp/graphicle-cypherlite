//! CIP-257: `SHOW CURRENT GRAPH TYPE AS GRAPH` -- renders the current graph
//! type as a set of virtual nodes/relationships suitable for visualization
//! (mirroring `db.schema.visualization`), rather than the CIP-180 canonical
//! specification string plain `SHOW CURRENT GRAPH TYPE` reports (see
//! `graph_type::current_graph_type_specification`).
//!
//! Node element type -> `(:NodeElementType {label, properties, constraints})`
//! Non-identifying label -> `(:NodeLabel {label, constraints})`
//! Unconstrained relationship endpoint -> `(:AnyLabel {})`
//! Edge element type -> `[:RELATIONSHIP_ELEMENT_TYPE {relationshipType, properties, constraints}]`
//! Non-identifying relationship type -> `[:RELATIONSHIP_TYPE {relationshipType, constraints}]`
//! Implied label -> `[:IMPLIES]`
//!
//! Every synthesized entity gets a **negative** id -- real, store-backed
//! nodes/relationships always start at 0 and count up, so a negative id
//! unambiguously marks "no real backing". Several other parts of the engine
//! use that same signal to treat a write against one of these as a no-op
//! (`DELETE`, property `SET`/`REMOVE` -- see `clause::delete`/`clause::set`)
//! or a hard failure (label `SET`/`REMOVE`, `CREATE` off of one -- these
//! fail naturally already, since the target id can never be found in the
//! store, or are explicitly guarded in `clause::remove`), matching CIP-257's
//! "Write behavior on virtual entities" (consistent with
//! `db.schema.visualization`).
//!
//! `startNode()`/`endNode()` (`eval::function`) need to resolve a virtual
//! relationship's endpoints without a store lookup, since these entities
//! were never inserted into the store; this module registers each
//! synthesized node with the graph layer's virtual-node side table
//! (`overlay::register_virtual_node`), which `ExecGraph::node_value`
//! consults as a fallback once the primary store/overlay lookup misses.

use std::collections::{BTreeMap, HashMap, HashSet};

use cypherlite_model::Constraint;
use cypherlite_model::CypherValue;
use cypherlite_parser::token::quote_identifier_shaped;
use cypherlite_parser::{
    render_canon, type_expr_to_canon, AnyNodeMode, GraphTypeBody, GraphTypeConstraintSpec,
    GraphTypeConstraintVerb, GraphTypeElementRef, GraphTypePropertyType,
};
use indexmap::IndexMap;

use crate::graph_type::{alias_tables, edge_id_label_set, node_id_label_set, resolve_ref_label};
use crate::overlay::register_virtual_node;

/// Renders one declared property as `"name :: TYPE"` (CIP-257's `properties`
/// list entries).
///
/// Every name position in the display form goes through the parser's
/// shape-only display policy ([`quote_identifier_shaped`]): bare for a plain
/// identifier shape, backtick-quoted otherwise (with embedded backticks
/// doubled). CIP-257's worked examples use this lighter, non-canonical text
/// rendering rather than CIP-180's `canonicalize_graph_type_body` (fewer
/// backticks, `n`/`r`-renamed constraint variables, an injected `=>`) --
/// `prop1`, not `` `prop1` ``, and `` `prop-3` `` for a hyphenated name, while
/// a keyword spelling such as `when` stays bare because display text is not
/// re-parsed as a statement.
fn vis_property_text(p: &GraphTypePropertyType) -> Result<String, cypherlite_core::Error> {
    let mut rendered = format!(
        "{} :: {}",
        quote_identifier_shaped(&p.name),
        render_canon(&type_expr_to_canon(&p.value_type))
    );
    // CIP-262: the property string carries the default value (ME5's
    // `id :: UUID NOT NULL DEFAULT ON COMMIT uuid()`), through the one
    // canonical default-value rendering the graph-type body form uses.
    if let Some(expr) = &p.default_value {
        // The same persisted-bytes rule as every stored-spec consumer
        // (the poisoned-spec siblings): an unrenderable default is a
        // typed storage error through the family's one mapping, never
        // a panic or a raw syntax error here.
        rendered.push_str(
            &crate::graph_type::canonical_default_clause(expr)
                .map_err(crate::graph_type::persisted_derive_error)?,
        );
    }
    Ok(rendered)
}

fn vis_target_str(is_edge: bool, label: &str, identified: bool) -> String {
    let var = if is_edge { "r" } else { "n" };
    match (is_edge, identified) {
        (true, true) => format!("()-[{}:{} =>]->()", var, quote_identifier_shaped(label)),
        (true, false) => format!("()-[{}:{}]->()", var, quote_identifier_shaped(label)),
        (false, true) => format!("({}:{} =>)", var, quote_identifier_shaped(label)),
        (false, false) => format!("({}:{})", var, quote_identifier_shaped(label)),
    }
}

/// CIP-257's `REQUIRE` clause: a single property is written bare
/// (`n.prop`), more than one is parenthesized (`(n.prop1, n.prop2)`) --
/// unlike CIP-180's canonical form, which always parenthesizes.
fn vis_props_str(is_edge: bool, properties: &[String]) -> String {
    let var = if is_edge { "r" } else { "n" };
    let items: Vec<String> = properties
        .iter()
        .map(|p| format!("{}.{}", var, quote_identifier_shaped(p)))
        .collect();
    if items.len() == 1 {
        items[0].clone()
    } else {
        format!("({})", items.join(", "))
    }
}

fn vis_verb_str(kind: &GraphTypeConstraintVerb, property_type: Option<&str>) -> String {
    match kind {
        GraphTypeConstraintVerb::Key => "IS KEY".to_string(),
        GraphTypeConstraintVerb::Unique => "IS UNIQUE".to_string(),
        GraphTypeConstraintVerb::Exists => "IS NOT NULL".to_string(),
        GraphTypeConstraintVerb::PropertyType => {
            format!("IS :: {}", property_type.unwrap_or_default())
        }
    }
}

/// Renders one graph-type-body `CONSTRAINT ...` element in CIP-257's
/// display form (see [`vis_property_text`]'s quoting note).
fn vis_constraint_text(
    c: &GraphTypeConstraintSpec,
    alias_table: &HashMap<&str, &str>,
    id_labels: &HashSet<&str>,
) -> String {
    let label = resolve_ref_label(&c.target, alias_table).unwrap_or_default();
    let identified = id_labels.contains(label.as_str());
    let target_str = vis_target_str(c.is_edge, &label, identified);
    let props_str = vis_props_str(c.is_edge, &c.properties);
    let property_type = c
        .property_type
        .as_ref()
        .map(|te| render_canon(&type_expr_to_canon(te)));
    let verb_str = vis_verb_str(&c.kind, property_type.as_deref());
    let name_part = c
        .name
        .as_deref()
        .map(|n| format!(" {}", quote_identifier_shaped(n)))
        .unwrap_or_default();
    format!("CONSTRAINT{name_part} FOR {target_str} REQUIRE {props_str} {verb_str}")
}

/// Renders an independently-created (store-only, not part of the current
/// graph type body) constraint the same way -- used for `extra_constraints`,
/// mirroring `graph_type::canonical_constraint_from_storage`: the one typed
/// storage-constraint renderer, in its display style.
fn vis_constraint_text_from_storage(c: &Constraint, identified: bool) -> String {
    crate::graph_type::render_storage_constraint(
        c,
        identified,
        &crate::graph_type::StorageConstraintStyle::DISPLAY,
    )
}

/// A registered node element type's own declared properties + the
/// undesignated constraints attached to it.
struct ElementEntry {
    properties: Vec<String>,
    constraints: Vec<String>,
}

/// Allocates the next (more negative) virtual id.
fn next_id(counter: &mut i64) -> i64 {
    let id = *counter;
    *counter -= 1;
    id
}

/// Resolves (allocating if needed) the any-node id for one unconstrained
/// relationship endpoint, honoring `mode`: `Single` shares exactly one
/// any-node across the whole graph type; `Separate` (the default) shares
/// one any-node per relationship (both its own endpoints, if both are
/// unconstrained) but never across different relationships.
fn any_node_id(
    mode: AnyNodeMode,
    counter: &mut i64,
    single_any_id: &mut Option<i64>,
    this_rel_any: &mut Option<i64>,
    any_node_ids: &mut Vec<i64>,
) -> i64 {
    let slot = match mode {
        AnyNodeMode::Single => single_any_id,
        AnyNodeMode::Separate => this_rel_any,
    };
    if let Some(id) = slot {
        *id
    } else {
        let id = next_id(counter);
        *slot = Some(id);
        any_node_ids.push(id);
        id
    }
}

/// Renders `body` (plus `extra_constraints` -- constraints in the store but
/// not part of `body` itself, from a plain `CREATE CONSTRAINT`) as CIP-257's
/// `nodes`/`relationships` column values.
pub fn render_graph_type_as_graph(
    body: &GraphTypeBody,
    extra_constraints: &[Constraint],
    mode: AnyNodeMode,
) -> Result<(Vec<CypherValue>, Vec<CypherValue>), cypherlite_core::Error> {
    let (node_alias_label, edge_alias_label) = alias_tables(body);
    let node_id_labels = node_id_label_set(body);
    let edge_id_labels = edge_id_label_set(body);

    // ---- Node side: identifying (element type) vs non-identifying (plain label) ----
    let mut element_types: BTreeMap<String, ElementEntry> = BTreeMap::new();
    let mut plain_labels: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for nt in &body.node_types {
        let mut properties = Vec::with_capacity(nt.properties.len());
        for p in &nt.properties {
            properties.push(vis_property_text(p)?);
        }
        element_types.insert(
            nt.identifying_label.clone(),
            ElementEntry {
                properties,
                constraints: Vec::new(),
            },
        );
        for implied in &nt.implied_labels {
            // CIP-266: an implied label that identifies another node
            // element type targets that type's own `NodeElementType`
            // node (it is already in `element_types` from its own
            // declaration) -- only a non-identifying implied label gets
            // a `NodeLabel` node (SGTE2).
            if node_id_labels.contains(implied.as_str()) {
                continue;
            }
            plain_labels.entry(implied.clone()).or_default();
        }
    }
    // A bare (non-`=>`) node endpoint reference on an edge type introduces
    // a plain label too, even with no other declaration or constraint of
    // its own (CIP-257 relationships[3]).
    for et in &body.edge_types {
        for endpoint in [&et.source, &et.target] {
            if let GraphTypeElementRef::InSitu {
                implies: false,
                label,
                ..
            } = endpoint
            {
                if !node_id_labels.contains(label.as_str()) {
                    plain_labels.entry(label.clone()).or_default();
                }
            }
        }
    }
    for c in body.constraints.iter().filter(|c| !c.is_edge) {
        let label = resolve_ref_label(&c.target, &node_alias_label).unwrap_or_default();
        if label.is_empty() {
            continue;
        }
        let text = vis_constraint_text(c, &node_alias_label, &node_id_labels);
        if node_id_labels.contains(label.as_str()) {
            if let Some(entry) = element_types.get_mut(&label) {
                entry.constraints.push(text);
            }
        } else {
            plain_labels.entry(label).or_default().push(text);
        }
    }
    for c in extra_constraints.iter().filter(|c| c.entity_type == "NODE") {
        let identified = node_id_labels.contains(c.label.as_str());
        let text = vis_constraint_text_from_storage(c, identified);
        if identified {
            if let Some(entry) = element_types.get_mut(&c.label) {
                entry.constraints.push(text);
            }
        } else {
            plain_labels.entry(c.label.clone()).or_default().push(text);
        }
    }

    // ---- Edge side: identifying (element type) vs non-identifying (plain relationship type) ----
    struct EdgeElement<'a> {
        properties: Vec<String>,
        constraints: Vec<String>,
        source: &'a GraphTypeElementRef,
        target: &'a GraphTypeElementRef,
    }
    let mut edge_elements: BTreeMap<String, EdgeElement> = BTreeMap::new();
    for et in &body.edge_types {
        let mut properties = Vec::with_capacity(et.properties.len());
        for p in &et.properties {
            properties.push(vis_property_text(p)?);
        }
        edge_elements.insert(
            et.identifying_label.clone(),
            EdgeElement {
                properties,
                constraints: Vec::new(),
                source: &et.source,
                target: &et.target,
            },
        );
    }
    let mut plain_rel_types: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for c in body.constraints.iter().filter(|c| c.is_edge) {
        let label = resolve_ref_label(&c.target, &edge_alias_label).unwrap_or_default();
        if label.is_empty() {
            continue;
        }
        let text = vis_constraint_text(c, &edge_alias_label, &edge_id_labels);
        if edge_id_labels.contains(label.as_str()) {
            if let Some(entry) = edge_elements.get_mut(&label) {
                entry.constraints.push(text);
            }
        } else {
            plain_rel_types.entry(label).or_default().push(text);
        }
    }
    for c in extra_constraints
        .iter()
        .filter(|c| c.entity_type == "RELATIONSHIP")
    {
        let identified = edge_id_labels.contains(c.label.as_str());
        let text = vis_constraint_text_from_storage(c, identified);
        if identified {
            if let Some(entry) = edge_elements.get_mut(&c.label) {
                entry.constraints.push(text);
            }
        } else {
            plain_rel_types
                .entry(c.label.clone())
                .or_default()
                .push(text);
        }
    }

    // ---- Allocate ids: named nodes first (needed to resolve edge endpoints) ----
    let mut counter: i64 = -1;
    let mut label_node_id: HashMap<String, i64> = HashMap::new();
    for label in element_types.keys() {
        label_node_id.insert(label.clone(), next_id(&mut counter));
    }
    for label in plain_labels.keys() {
        label_node_id.insert(label.clone(), next_id(&mut counter));
    }

    // ---- Resolve every relationship's endpoints (allocating any-nodes as needed) ----
    #[allow(clippy::type_complexity)]
    let mut rel_specs: Vec<(String, bool, i64, i64, i64, Vec<String>, Vec<String>)> = Vec::new();
    let mut single_any_id: Option<i64> = None;
    let mut any_node_ids: Vec<i64> = Vec::new();

    for (label, entry) in &edge_elements {
        let rel_id = next_id(&mut counter);
        let mut this_rel_any: Option<i64> = None;
        let src_id = match resolve_ref_label(entry.source, &node_alias_label) {
            Some(l) => *label_node_id.get(&l).unwrap_or(&0),
            None => any_node_id(
                mode,
                &mut counter,
                &mut single_any_id,
                &mut this_rel_any,
                &mut any_node_ids,
            ),
        };
        let tgt_id = match resolve_ref_label(entry.target, &node_alias_label) {
            Some(l) => *label_node_id.get(&l).unwrap_or(&0),
            None => any_node_id(
                mode,
                &mut counter,
                &mut single_any_id,
                &mut this_rel_any,
                &mut any_node_ids,
            ),
        };
        rel_specs.push((
            label.clone(),
            true,
            rel_id,
            src_id,
            tgt_id,
            entry.properties.clone(),
            entry.constraints.clone(),
        ));
    }
    for (label, constraints) in &plain_rel_types {
        let rel_id = next_id(&mut counter);
        let mut this_rel_any: Option<i64> = None;
        let src_id = any_node_id(
            mode,
            &mut counter,
            &mut single_any_id,
            &mut this_rel_any,
            &mut any_node_ids,
        );
        let tgt_id = any_node_id(
            mode,
            &mut counter,
            &mut single_any_id,
            &mut this_rel_any,
            &mut any_node_ids,
        );
        rel_specs.push((
            label.clone(),
            false,
            rel_id,
            src_id,
            tgt_id,
            Vec::new(),
            constraints.clone(),
        ));
    }

    // ---- Assemble the `nodes` list: any-nodes, then NodeElementType, then NodeLabel (each alphabetical) ----
    let mut nodes: Vec<CypherValue> =
        Vec::with_capacity(any_node_ids.len() + element_types.len() + plain_labels.len());
    for id in &any_node_ids {
        let v = CypherValue::Node {
            id: *id,
            labels: vec!["AnyLabel".to_string()],
            properties: IndexMap::new(),
        };
        register_virtual_node(*id, v.clone());
        nodes.push(v);
    }
    for (label, entry) in &element_types {
        let id = *label_node_id.get(label).unwrap();
        let mut props = IndexMap::new();
        props.insert("label".to_string(), CypherValue::Str(label.clone()));
        props.insert(
            "properties".to_string(),
            CypherValue::List(
                entry
                    .properties
                    .iter()
                    .cloned()
                    .map(CypherValue::Str)
                    .collect(),
            ),
        );
        props.insert(
            "constraints".to_string(),
            CypherValue::List(
                entry
                    .constraints
                    .iter()
                    .cloned()
                    .map(CypherValue::Str)
                    .collect(),
            ),
        );
        let v = CypherValue::Node {
            id,
            labels: vec!["NodeElementType".to_string()],
            properties: props,
        };
        register_virtual_node(id, v.clone());
        nodes.push(v);
    }
    for (label, constraints) in &plain_labels {
        let id = *label_node_id.get(label).unwrap();
        let mut props = IndexMap::new();
        props.insert("label".to_string(), CypherValue::Str(label.clone()));
        props.insert(
            "constraints".to_string(),
            CypherValue::List(constraints.iter().cloned().map(CypherValue::Str).collect()),
        );
        let v = CypherValue::Node {
            id,
            labels: vec!["NodeLabel".to_string()],
            properties: props,
        };
        register_virtual_node(id, v.clone());
        nodes.push(v);
    }

    // ---- Assemble the `relationships` list: RELATIONSHIP_ELEMENT_TYPE, then
    // RELATIONSHIP_TYPE (each alphabetical, from `rel_specs`'s own build
    // order), then IMPLIES ----
    let mut relationships: Vec<CypherValue> = Vec::with_capacity(rel_specs.len());
    for (label, is_element, rel_id, src_id, tgt_id, properties, constraints) in &rel_specs {
        let mut props = IndexMap::new();
        props.insert(
            "relationshipType".to_string(),
            CypherValue::Str(label.clone()),
        );
        if *is_element {
            props.insert(
                "properties".to_string(),
                CypherValue::List(properties.iter().cloned().map(CypherValue::Str).collect()),
            );
        }
        props.insert(
            "constraints".to_string(),
            CypherValue::List(constraints.iter().cloned().map(CypherValue::Str).collect()),
        );
        let rel_type = if *is_element {
            "RELATIONSHIP_ELEMENT_TYPE"
        } else {
            "RELATIONSHIP_TYPE"
        };
        relationships.push(CypherValue::Relationship {
            id: *rel_id,
            start_id: *src_id,
            end_id: *tgt_id,
            rel_type: rel_type.to_string(),
            properties: props,
        });
    }
    for nt in &body.node_types {
        let Some(&src_id) = label_node_id.get(&nt.identifying_label) else {
            continue;
        };
        for implied in &nt.implied_labels {
            let Some(&tgt_id) = label_node_id.get(implied) else {
                continue;
            };
            let id = next_id(&mut counter);
            relationships.push(CypherValue::Relationship {
                id,
                start_id: src_id,
                end_id: tgt_id,
                rel_type: "IMPLIES".to_string(),
                properties: IndexMap::new(),
            });
        }
    }

    Ok((nodes, relationships))
}
