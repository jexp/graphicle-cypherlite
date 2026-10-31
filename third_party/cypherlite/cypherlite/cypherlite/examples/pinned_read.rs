use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use cypherlite::{Result, Value};
use cypherlite_executor::{
    execute, execute_read_at, ExecGraph, ExecRequest, Graph, ReadAdmission, RedoOp, WriteScratch,
};
use cypherlite_front::parse_and_lower_statement;
use cypherlite_planner::plan;

fn main() -> Result<()> {
    let graph = Graph::default();
    let create = |id| RedoOp::CreateNode {
        id,
        labels: vec!["N".into()].into(),
        properties: cypherlite::PropertyMap::share(BTreeMap::new()),
    };
    graph.apply_redo(
        vec![
            create(0),
            RedoOp::CreateIndex {
                name: "old_index".into(),
                label: "N".into(),
                property: "v".into(),
            },
        ],
        1,
    )?;
    let pin = graph.pin_now()?;
    let schema = graph.schema_snapshot();
    graph.apply_redo(
        vec![
            create(1),
            RedoOp::CreateIndex {
                name: "new_index".into(),
                label: "N".into(),
                property: "w".into(),
            },
        ],
        2,
    )?;

    let query = "MATCH (n:N) RETURN count(n) AS total";
    let logical = plan(parse_and_lower_statement(query)?.query, &mut |_| false)?;
    let result = execute_read_at(&ReadAdmission {
        graph: &graph,
        snapshot_ts: pin.ts(),
        schema: &schema,
        request: ExecRequest::new(&logical, &BTreeMap::new(), query),
        cancel: None,
        ontology_document: None,
    })?;
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
    let query = "SHOW INDEXES YIELD name RETURN name ORDER BY name";
    let logical = plan(parse_and_lower_statement(query)?.query, &mut |_| false)?;
    let result = execute_read_at(&ReadAdmission {
        graph: &graph,
        snapshot_ts: pin.ts(),
        schema: &schema,
        request: ExecRequest::new(&logical, &BTreeMap::new(), query),
        cancel: None,
        ontology_document: None,
    })?;
    assert_eq!(result.rows, vec![vec![Value::String("old_index".into())]]);

    // The public view constructors accept the same admitted schema. A write
    // view folds its private redo over that schema without changing the graph.
    let view = ExecGraph::read_at(&graph, pin.ts()).with_schema(&schema);
    let result = execute(&view, &ExecRequest::new(&logical, &BTreeMap::new(), query))?;
    assert_eq!(result.rows, vec![vec![Value::String("old_index".into())]]);
    let scratch = RefCell::new(WriteScratch::new(
        pin.ts(),
        Arc::new(AtomicU64::new(2)),
        Arc::new(AtomicU64::new(0)),
    ));
    scratch.borrow_mut().push_redo(RedoOp::DropIndex {
        name: "old_index".into(),
    });
    let view = ExecGraph::write(&graph, &scratch).with_schema(&schema);
    assert!(
        execute(&view, &ExecRequest::new(&logical, &BTreeMap::new(), query),)?
            .rows
            .is_empty()
    );

    // Derived SHOW metadata is a redo operation too: it is published with
    // its constraint, never by a speculative write to the shared theater.
    graph.apply_redo(
        vec![
            RedoOp::CreateConstraint {
                name: "annotated".into(),
                relationship: false,
                label: "N".into(),
                properties: vec![],
                kind: "nodeLabelExistence".into(),
                property_type: None,
            },
            RedoOp::CatalogDerivedConstraint {
                constraint: cypherlite_model::Constraint {
                    name: "annotated".into(),
                    label: "N".into(),
                    properties: vec![],
                    constraint_type: cypherlite_model::ConstraintType::NodeLabelExistence,
                    property_type: None,
                    entity_type: "NODE".into(),
                    enforced_label: Some("Required".into()),
                },
            },
        ],
        3,
    )?;
    let latest_pin = graph.pin_now()?;
    let latest = graph.schema_snapshot();
    let query = "SHOW CONSTRAINTS YIELD name, enforcedLabel RETURN name, enforcedLabel";
    let logical = plan(parse_and_lower_statement(query)?.query, &mut |_| false)?;
    let result = execute_read_at(&ReadAdmission {
        graph: &graph,
        snapshot_ts: latest_pin.ts(),
        schema: &latest,
        request: ExecRequest::new(&logical, &BTreeMap::new(), query),
        cancel: None,
        ontology_document: None,
    })?;
    assert_eq!(
        result.rows,
        vec![vec![
            Value::String("annotated".into()),
            Value::String("Required".into())
        ]]
    );
    // Storage codecs save the typed theater beside the named catalog; on
    // recovery they install definitions before restoring this image.
    let recovered = Graph::default();
    for definition in graph.named_constraints() {
        recovered.install_named_constraint(definition)?;
    }
    recovered.restore_checkpoint_theater(graph.checkpoint_theater_for_write()?)?;
    assert_eq!(
        recovered.checkpoint_theater().enforced_label("annotated"),
        Some("Required")
    );
    Ok(())
}
