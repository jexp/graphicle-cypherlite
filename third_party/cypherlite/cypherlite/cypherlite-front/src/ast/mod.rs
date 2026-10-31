//! The low, shared AST layer every semantic pass builds on. It sits below
//! `validate`, `call_scope`, `local_callables` and `lower`, which all
//! consume it downward — none of them owns it.
//!
//! - [`collect`]: the pure collectors (variables, patterns, names, string
//!   interpolation) plus the shared expression-scope check pair
//!   (`check_expr_vars_defined`/`check_name_defined`) and the UNION-chain
//!   enumeration (`union_arms`/`union_chain`) — neutral plumbing, not
//!   validator policy.
//! - [`search_spec`]: the one walk over a SEARCH source specification.
//! - [`stmt_ref`]: the statement-position type (real statement, UNION arm,
//!   NEXT stage, or FOREACH body) the whole-tree walkers visit.
//! - [`tree`]: the whole-tree statement/expression walker family and the
//!   ADR-0257 nesting-depth cap they enforce.

pub(crate) mod collect;
pub(crate) mod search_spec;
pub(crate) mod stmt_ref;
pub(crate) mod tree;

/// The shared nesting-depth cap, the parser's own constant (ADR-0257): the
/// structural walkers refuse a statement or expression nested deeper.
pub const MAX_NESTING_DEPTH: usize = cypherlite_parser::parser::MAX_NESTING_DEPTH as usize;
