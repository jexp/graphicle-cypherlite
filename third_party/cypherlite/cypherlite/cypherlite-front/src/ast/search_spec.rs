//! The one walker over the parser-AST `<source specification clause>`
//! tree (CIP-224/CIP-278), with its grammar-slot tag.

use cypherlite_error::EngineError;
use cypherlite_parser::{Expr, SearchIndexSpec, SearchSpec};

/// One grammar slot of a `<source specification clause>` expression, the
/// tag [`walk_search_spec`] yields -- the slot identity consumers that
/// must treat one slot differently (the CIP-278 Rule 6 control check
/// selects WEIGHT/CONSTANT only) match on instead of hand-rolling a
/// second recursion over the same tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchSlot {
    For,
    Analyzer,
    InnerWhere,
    Offset,
    Limit,
    Weight,
    Constant,
}

/// The one walker over the parser-AST `<source specification clause>`
/// tree: every index specification (recursing through nested COMBINE
/// sources), each with every expression slot it carries, tagged with the
/// slot. The one grammar walk that can reject: both callbacks may fail,
/// and the first failure stops the walk (an over-deep expression, a CIP-224
/// rule violation). The only other traversal is `local_callables`'
/// mutable rewrite visitor, which mirrors this slot list deliberately (it
/// must rebuild the tree, not just visit it); a new slot must be added to
/// both. The IR
/// mirrors it (`cypherlite_ir::SearchSpec::exprs`) -- the two operate on
/// mirrored types, so a new slot must be added to both.
pub(crate) fn walk_search_spec<'a>(
    spec: &'a SearchSpec,
    on_index: &mut impl FnMut(&'a SearchIndexSpec) -> Result<(), EngineError>,
    on_expr: &mut impl FnMut(SearchSlot, &'a Expr) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    match spec {
        SearchSpec::Index(index) => {
            on_index(index)?;
            on_expr(SearchSlot::For, &index.for_expr)?;
            if let Some(expr) = &index.analyzer {
                on_expr(SearchSlot::Analyzer, expr)?;
            }
            if let Some(expr) = &index.inner_where {
                on_expr(SearchSlot::InnerWhere, expr)?;
            }
            if let Some(expr) = &index.offset_expr {
                on_expr(SearchSlot::Offset, expr)?;
            }
            on_expr(SearchSlot::Limit, &index.limit_expr)?;
        }
        SearchSpec::Combine(combine) => {
            for source in &combine.sources {
                walk_search_spec(&source.spec, on_index, on_expr)?;
                if let Some(weight) = &source.weight_expr {
                    on_expr(SearchSlot::Weight, weight)?;
                }
            }
            if let Some(constant) = &combine.constant_expr {
                on_expr(SearchSlot::Constant, constant)?;
            }
            if let Some(limit) = &combine.limit_expr {
                on_expr(SearchSlot::Limit, limit)?;
            }
        }
    }
    Ok(())
}
