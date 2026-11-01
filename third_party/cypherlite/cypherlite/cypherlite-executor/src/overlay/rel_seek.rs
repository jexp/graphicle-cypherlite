//! Relationship equality seeks over the overlay (ADR-0185): the committed
//! graph's lazy relationship postings plus the scratch twin.

use cypherlite_core::{RelId, Value};

use super::ExecGraph;

impl<'a> ExecGraph<'a> {
    /// Candidate ids for a relationship uniqueness probe on
    /// `(rel_type, key) = value` (ADR-0185), ascending and deduplicated: the
    /// committed graph's lazy equality postings for the pair plus the
    /// scratch's `rel_prop_ix` seek, merged through
    /// [`ExecGraph::merged_candidates`]. A superset — the caller re-verifies
    /// every id on this view. Empty when `value` has no property-index key
    /// (it contains a NaN).
    pub(crate) fn rel_equality_candidates(
        &self,
        rel_type: &str,
        key: &str,
        value: &Value,
    ) -> Vec<RelId> {
        self.merged_candidates(self.base.rel_equality_postings(rel_type, key, value), |s| {
            s.batch
                .rel_prop_ix
                .seek(&s.batch.rels, rel_type, key, value)
        })
    }

    /// The committed-plus-scratch bucket length for `(rel_type, key) = value`
    /// (ADR-0184, ADR-0185), for choosing the smallest bucket: the committed
    /// postings' length plus the scratch seek's length, both counted in
    /// place through [`ExecGraph::merged_len`].
    pub(crate) fn rel_equality_bucket_len(
        &self,
        rel_type: &str,
        key: &str,
        value: &Value,
    ) -> usize {
        self.merged_len(
            self.base.rel_equality_postings_len(rel_type, key, value),
            |s| {
                s.batch
                    .rel_prop_ix
                    .seek_len(&s.batch.rels, rel_type, key, value)
            },
        )
    }
}

#[cfg(test)]
mod tests;
