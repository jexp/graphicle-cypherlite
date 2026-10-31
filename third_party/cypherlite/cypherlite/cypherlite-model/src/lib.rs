//! CypherLite data model types.
//!
//! Defines the core value type system used throughout CypherLite,
//! including the `CypherValue` enum that replaces Go's `interface{}`.

pub mod canonical_key;
pub mod schema;
pub mod serde_value;
pub mod value;
pub mod wire;

pub use indexmap::IndexMap;
pub use schema::{
    Constraint, ConstraintClassification, ConstraintType, EnforcedLabelSite, EntityKind,
};
pub use value::{CypherValue, GeometryValue, VectorCoordKind};
