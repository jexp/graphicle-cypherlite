mod catalog_meta;
mod csv;
mod ctx;
mod effect;
mod eval;
mod exec;
mod function_registry;
mod graph;
mod graph_type;
mod graph_type_visualize;
mod group_key;
#[cfg(test)]
mod group_key_tests;
mod memory;
mod overlay;
// Private module with a root re-export only (one public path): internal
// callers use `crate::procedure_gate::…`, external consumers use the root
// re-export below.
mod procedure_gate;
mod workspace;
// Private module with root re-exports only (one public path): internal
// callers already use `crate::procedure_registry::…`, external consumers
// use the root re-exports below.
mod procedure_registry;
mod pull_identity;
mod redo;
mod shortest;
mod show_catalog;
mod spatial;
mod store;
mod vector_similarity;

pub use catalog_meta::{DeprecatedBy, DeprecationForm};
pub use csv::CsvPolicy;
pub use ctx::{
    BatchCommit, ClauseMetricsRecord, PhaseTimes, PlanMetricsSink, StartReason, StoreReads,
    WalkStart,
};
pub use cypherlite_catalog::IndexDef;
pub use effect::StatementEffect;
pub use exec::bulk::{
    analyze_bulk_write, BulkCursor, BulkTargets, BulkWrite, DEFAULT_BATCH_BUDGET,
    DEFAULT_BATCH_THRESHOLD,
};
pub use exec::graph_stats::GraphStats;
pub use exec::procedures::{check_explicit_arity, check_implicit_passing_mode};
pub use exec::{
    execute, execute_read_at, value_matches_proc_type, ExecRequest, QueryResult, ReadAdmission,
};
pub use function_registry::builtin_function_deprecation;
pub use graph::{
    CheckpointTheater, DatabaseEntry, Graph, Procedure, QueryStats, SchemaSnapshot, SnapshotPin,
};
pub use graph_type::GraphTypeSpec;
pub use overlay::{ExecGraph, WriteScratch};
// The ADR-0395 budget constant: workspace-internal (test-support's
// single-sourced assertion reads it), deliberately not advertised API —
// the budget is a fixed contract, not a knob (ADR-0395: "never ambient
// configuration").
#[doc(hidden)]
pub use overlay::NESTED_EVAL_BUDGET;
pub use procedure_gate::procedure_preview_feature;
pub use procedure_registry::{
    builtin_procedure_deprecation, builtin_procedure_signature, call_is_write,
    is_ontology_procedure, is_ontology_store_op, is_ontology_write_procedure, ProcSignature,
};
pub use redo::{
    legacy_property_encoding, props_from_json, split_legacy_property_encoding, IndexDetailSpec,
    IndexKindSpec, OntologyOp, PullKind, RedoOp, MAX_ONTOLOGY_SPEC_BYTES,
};
#[doc(hidden)]
pub use workspace::result_rows_bytes;
