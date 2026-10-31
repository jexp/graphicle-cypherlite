//! Cardinality hints for MATCH pattern ordering.
//!
//! The planner does not own the graph. Callers that can see label sizes,
//! index hit counts, and relationship-type counts pass them in. A missing
//! count (`None`) makes ordering fall back to the fixed rank of that
//! endpoint.

use cypherlite_ir::Expr;

/// Live or stubbed counts used to estimate how many rows a pattern endpoint
/// produces. Every method returns `None` when that signal is unavailable,
/// which is not the same as zero rows.
pub trait PatternStats {
    /// Nodes carrying `label`.
    fn label_rows(&self, label: &str) -> Option<u64>;

    /// Hits of an equality seek on a RANGE index for `(label, property)`.
    /// `value` is the raw predicate expression. Implementors resolve
    /// literals and parameters with [`Expr::seek_value`] and return `None`
    /// for anything else. `None` also means no RANGE index, not zero hits.
    fn equality_rows(&self, label: &str, property: &str, value: &Expr) -> Option<u64>;

    /// Candidates of a `CONTAINS` seek on a TEXT index for `(label, property)`.
    /// `needle` is the raw predicate expression. Implementors resolve
    /// literals and parameters with [`Expr::seek_value`] and return `None`
    /// for anything else. `None` also means no TEXT index, not zero candidates.
    fn contains_rows(&self, label: &str, property: &str, needle: &Expr) -> Option<u64>;

    /// Relationships of `rel_type`.
    fn rel_type_rows(&self, rel_type: &str) -> Option<u64>;

    /// LABS-351: the tail of a (label, rel-type) out-degree distribution —
    /// the conservative floor of the highest observed degree (a log2-bucket
    /// histogram's top bucket). `None` when the stats keep no histogram.
    /// Consumed by `apply_degree` (LABS-426, the mean-to-tail flip): an
    /// outgoing hop's fanout rides the tail when the histogram has one,
    /// the mean otherwise — the plan-choice consequence of the flip is
    /// the measured part of that change.
    fn degree_tail(&self, label: &str, rel_type: &str) -> Option<u64> {
        let _ = (label, rel_type);
        None
    }
}

/// No counts. Ordering uses the bound / equality / label / bare ranks.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPatternStats;

impl PatternStats for NoPatternStats {
    fn label_rows(&self, _label: &str) -> Option<u64> {
        None
    }

    fn equality_rows(&self, _label: &str, _property: &str, _value: &Expr) -> Option<u64> {
        None
    }

    fn contains_rows(&self, _label: &str, _property: &str, _needle: &Expr) -> Option<u64> {
        None
    }

    fn rel_type_rows(&self, _rel_type: &str) -> Option<u64> {
        None
    }
}
