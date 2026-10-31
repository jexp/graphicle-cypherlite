//! Point builtins: thin wrappers over the `crate::spatial` helpers.
//! `point()` resolves node/relationship arguments to fresh properties by
//! delegating to `properties()` (ADR-0167, LABS-169).

use cypherlite_core::{Result, Value};

use crate::overlay::ExecGraph;
use crate::spatial::{point_construct, point_distance, point_within_bbox, point_within_distance};

use super::entity::bi_properties;
#[cfg(test)]
use std::sync::Arc;

pub(super) fn bi_point(graph: &ExecGraph<'_>, n: &str, vals: Vec<Value>) -> Result<Value> {
    // ADR-0167 (LABS-169): nodes and relationships are map-like point()
    // arguments; resolve them to fresh properties exactly like properties().
    match vals.first() {
        Some(Value::Node(_)) | Some(Value::Relationship(_)) => {
            point_construct(&[bi_properties(graph, n, vals)?])
        }
        _ => point_construct(&vals),
    }
}

pub(super) fn bi_distance(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    point_distance(&vals)
}

pub(super) fn bi_point_withinbbox(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    point_within_bbox(&vals)
}

pub(super) fn bi_point_withindistance(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    point_within_distance(&vals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use std::collections::BTreeMap;

    #[test]
    fn point_resolves_node_and_relationship_arguments_through_the_graph() {
        let g = Graph::default();
        let mut geo = BTreeMap::new();
        geo.insert("longitude".to_string(), Value::Float(18.0686));
        geo.insert("latitude".to_string(), Value::Float(59.3293));
        geo.insert("address".to_string(), Value::String("5 Corrie Road".into()));
        let geo_id = g.create_node(vec!["Location".to_string()], geo).unwrap();
        let mut cart = BTreeMap::new();
        cart.insert("x".to_string(), Value::Int(1));
        cart.insert("y".to_string(), Value::Int(2));
        let cart_id = g.create_node(vec!["Location".to_string()], cart).unwrap();
        let mut rel_props = BTreeMap::new();
        rel_props.insert("x".to_string(), Value::Int(0));
        rel_props.insert("y".to_string(), Value::Int(0));
        rel_props.insert("z".to_string(), Value::Int(5));
        let rel_id = g
            .create_rel(geo_id, cart_id, "NEAR".to_string(), rel_props)
            .unwrap();
        let graph = ExecGraph::read(&g);

        let point_of = |v: Value| bi_point(&graph, "point", vec![v]).unwrap();
        let Value::Point(p) = point_of(Value::from_node(graph.node(geo_id).unwrap())) else {
            panic!("expected point")
        };
        assert_eq!((p.srid, p.x, p.y, p.z), (4326, 18.0686, 59.3293, None));
        let Value::Point(p) = point_of(Value::from_node(graph.node(cart_id).unwrap())) else {
            panic!("expected point")
        };
        assert_eq!((p.srid, p.x, p.y, p.z), (7203, 1.0, 2.0, None));
        let Value::Point(p) = point_of(Value::from_relationship(graph.rel(rel_id).unwrap())) else {
            panic!("expected point")
        };
        assert_eq!((p.srid, p.x, p.y, p.z), (9157, 0.0, 0.0, Some(5.0)));
    }

    #[test]
    fn point_reads_fresh_entity_properties() {
        let g = Graph::default();
        let mut props = BTreeMap::new();
        props.insert("x".to_string(), Value::Int(3));
        props.insert("y".to_string(), Value::Int(4));
        let id = g.create_node(vec!["L".to_string()], props).unwrap();
        let graph = ExecGraph::read(&g);
        // A stale Value::Node snapshot still resolves to the live properties.
        let stale = Value::from_node(cypherlite_core::Node {
            id,
            labels: Arc::new(vec!["L".to_string()]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        });
        let Value::Point(p) = bi_point(&graph, "point", vec![stale]).unwrap() else {
            panic!("expected point")
        };
        assert_eq!((p.srid, p.x, p.y), (7203, 3.0, 4.0));
    }

    #[test]
    fn point_on_a_missing_entity_errors_like_an_empty_map() {
        let g = Graph::default();
        let graph = ExecGraph::read(&g);
        let missing = Value::from_node(cypherlite_core::Node {
            id: 4242,
            labels: Arc::new(vec![]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        });
        let err = bi_point(&graph, "point", vec![missing])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("'x' and 'y', or 'latitude' and 'longitude'"),
            "unexpected error: {err}"
        );
    }
}
