//! `+labels` / `-labels` accounting (ADR-0171): whether a label a write gains
//! or loses enters or leaves the live graph, answered from the scratch's
//! `live_label_delta` and at most one snapshot probe per label
//! (`graph::LabelProbe`).

use super::{distinct_labels, ExecGraph, WriteScratch};

/// Whether a write gives a node the labels being accounted or takes them away.
#[derive(Clone, Copy)]
pub(super) enum LabelChange {
    /// `CREATE`, MERGE-create, `SET n:L`: the node does not carry the label.
    Gain,
    /// `DELETE`, `DETACH DELETE`, `REMOVE n:L`: the node carries the label.
    Loss,
}

impl LabelChange {
    /// Live carriers, `S + D` before the write, at which the write leaves
    /// the label's liveness unchanged (ADR-0171 rules 2-3): one for a gain
    /// (it is live already), two for a loss (another carrier remains).
    fn need(self) -> i64 {
        match self {
            Self::Gain => 1,
            Self::Loss => 2,
        }
    }

    fn sign(self) -> i64 {
        match self {
            Self::Gain => 1,
            Self::Loss => -1,
        }
    }
}

impl ExecGraph<'_> {
    /// Net `(+labels, -labels)` of `change` on each distinct label in
    /// `labels`, accounted against the borrowed scratch `s` — label
    /// accounting only ever runs on a write view, so a read-only view is a
    /// type error here, not a panic. A label whose liveness the write
    /// leaves unchanged counts nothing. For one that enters (gain) or
    /// leaves (loss) the live graph, the write undoes or makes a removal
    /// if the snapshot carried it, and makes or undoes an addition
    /// otherwise.
    pub(super) fn label_deltas(
        &self,
        s: &WriteScratch,
        labels: &[String],
        change: LabelChange,
    ) -> (i64, i64) {
        let sign = change.sign();
        let (mut added, mut removed) = (0i64, 0i64);
        for l in distinct_labels(labels) {
            let k = change.need() - s.live_label_delta(l);
            match s.batch.label_probe.carriers_below(self.base, l, k) {
                None => {}
                Some(0) => added += sign,
                Some(_) => removed -= sign,
            }
        }
        (added, removed)
    }
}

#[cfg(test)]
mod tests;
