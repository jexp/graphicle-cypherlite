//! The fold's streaming accumulators (ADR-0036): `StreamAgg` (shared by
//! the one-hop fold and the multi-hop path fold) and `DistinctAcc` (the
//! distinct set with its integer fast path) — extracted so mod.rs keeps
//! the fold entry points, seek policy, and output glue.
//!
//! Visibility policy: every item and method in this file is
//! `pub(in crate::exec)` — the consumers are the fold modules
//! (`agg_fold`, `agg_fold::path`) and the sibling `pushdown_tests.rs`,
//! all inside `crate::exec`.

use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{Result, Value};
use cypherlite_ir::{classify_aggregate, AggregateKind, Expr};
use cypherlite_model::canonical_key::f64_exact_i64;

use super::super::aggregate::SumAcc;
use crate::eval::{eval, Row};
use crate::overlay::ExecGraph;

use crate::group_key::GroupKey;

/// One streaming aggregate, shared by both folds: the one-hop fold
/// classifies a projection expression once in [`StreamAgg::new`] and
/// offers rows through [`StreamAgg::offer`]; the path fold extracts the
/// value itself and offers it through [`StreamAgg::offer_value`]. Both
/// drain through the single [`StreamAgg::finish`].
#[derive(Clone)]
pub(in crate::exec) enum StreamAgg {
    CountStar(i64),
    Count(i64),
    CountDistinct(DistinctAcc),
    Sum(SumAcc),
}

impl StreamAgg {
    /// Classify the expression once, consulting the engine's canonical
    /// aggregate contract (ADR-0217, [`classify_aggregate`]) and matching
    /// the typed kind instead of re-sniffing name strings: a call that
    /// misses the contract (including the zero-argument `count()` — the
    /// star form is the separate [`Expr::CountStar`] variant) declines
    /// the fold, so the general path states the contract's rejection.
    /// Returns the accumulator paired with its validated operand (`None`
    /// for `count(*)`).
    pub(in crate::exec) fn new(expr: &Expr) -> Option<(Self, Option<&Expr>)> {
        match expr {
            Expr::CountStar => Some((Self::count_star(), None)),
            Expr::Function {
                name,
                distinct,
                args,
            } => {
                let kind = classify_aggregate(name)?;
                if args.len() != kind.arity().required() {
                    return None;
                }
                let agg = match kind {
                    AggregateKind::Count if *distinct => Self::count_distinct(),
                    AggregateKind::Count if !*distinct => Self::count(),
                    AggregateKind::Sum if !*distinct => Self::Sum(SumAcc::new()),
                    _ => return None,
                };
                Some((agg, args.first()))
            }
            _ => None,
        }
    }

    /// The row-count prototype (`count(*)` and the argument-free
    /// `count()`): variant construction stays here so callers never reach
    /// into the accumulator's representation.
    pub(in crate::exec) fn count_star() -> Self {
        Self::CountStar(0)
    }

    /// The non-null-count prototype (`count(operand)`): variant construction
    /// stays here so callers never reach into the accumulator's
    /// representation.
    pub(in crate::exec) fn count() -> Self {
        Self::Count(0)
    }

    /// The distinct-count prototype (the path fold's `count(DISTINCT …)`
    /// seat): variant construction stays here so callers never reach into
    /// the accumulator's representation.
    pub(in crate::exec) fn count_distinct() -> Self {
        Self::CountDistinct(DistinctAcc::default())
    }

    /// The one-hop fold's row channel: evaluate the operand captured at
    /// classification and feed the shared value channel. The variant
    /// already carries the classification — no per-row re-derivation.
    pub(in crate::exec) fn offer(
        &mut self,
        graph: &ExecGraph<'_>,
        row: &Row,
        operand: Option<&Expr>,
        params: &BTreeMap<String, Value>,
        cypher_version: Option<u32>,
    ) -> Result<()> {
        let value = match operand {
            None => None,
            Some(e) => Some(eval(graph, row, e, params, cypher_version)?),
        };
        self.offer_value(value.as_ref())
    }

    /// The shared value channel (the path fold offers through it
    /// directly): `None` offers no operand value — a `count(*)` row, or
    /// the path fold's argument-free `count()`. Counts skip nulls;
    /// distinct and sum insert the value. Fallible because the `Sum` arm
    /// routes through [`SumAcc::offer`].
    pub(in crate::exec) fn offer_value(&mut self, value: Option<&Value>) -> Result<()> {
        match self {
            Self::CountStar(n) => *n += 1,
            Self::Count(n) => {
                if value.is_none_or(|v| !v.is_null()) {
                    *n += 1;
                }
            }
            Self::CountDistinct(acc) => {
                if let Some(v) = value {
                    acc.insert(v)?;
                }
            }
            Self::Sum(acc) => {
                if let Some(v) = value {
                    acc.offer(v, "sum")?;
                }
            }
        }
        Ok(())
    }

    pub(in crate::exec) fn finish(&self) -> Value {
        match self {
            Self::CountStar(n) | Self::Count(n) => Value::Int(*n),
            Self::CountDistinct(acc) => Value::Int(acc.len()),
            Self::Sum(acc) => acc.clone().finish_sum(),
        }
    }
}

fn exact_int(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        Value::Float(f) => f64_exact_i64(*f),
        _ => None,
    }
}

/// Exact integers share one bucket (`1` and `1.0`). Anything else moves the
/// accumulator onto [`GroupKey`], carrying the integers already seen.
enum DistinctKind {
    Ints(HashSet<i64>),
    Keys(HashSet<GroupKey>),
}

impl DistinctKind {
    fn bytes(&self) -> u64 {
        match self {
            Self::Ints(set) => crate::workspace::int_set_bytes(set),
            Self::Keys(set) => crate::workspace::group_key_set_bytes(set),
        }
    }

    fn clone_set(&self) -> Self {
        match self {
            Self::Ints(set) => Self::Ints(set.clone()),
            Self::Keys(set) => Self::Keys(set.clone()),
        }
    }
}

/// The distinct set plus the workspace reservation for its own entries.
/// A duplicate offer does not grow the reservation. Cloning an empty
/// prototype does not take a second reservation; cloning a filled set does.
pub(in crate::exec) struct DistinctAcc {
    kind: DistinctKind,
    charge: cypherlite_core::tokens::WorkspaceCharge,
}

impl Clone for DistinctAcc {
    fn clone(&self) -> Self {
        let kind = self.kind.clone_set();
        let charge = cypherlite_core::tokens::WorkspaceCharge::retain_with(|| kind.bytes())
            .unwrap_or_else(|_| cypherlite_core::tokens::WorkspaceCharge::empty());
        Self { kind, charge }
    }
}

impl Default for DistinctAcc {
    /// The integer fast path is the initial state.
    fn default() -> Self {
        Self {
            kind: DistinctKind::Ints(HashSet::new()),
            charge: cypherlite_core::tokens::WorkspaceCharge::empty(),
        }
    }
}

impl DistinctAcc {
    fn sync(&mut self) -> Result<()> {
        self.charge.resize_with(|| self.kind.bytes())?;
        cypherlite_core::tokens::peak_hold("distinct");
        Ok(())
    }

    fn charge_delta(&mut self, delta: u64) -> Result<()> {
        if !cypherlite_core::tokens::workspace_accounting() || delta == 0 {
            return Ok(());
        }
        self.charge
            .resize(self.charge.bytes().saturating_add(delta))?;
        cypherlite_core::tokens::peak_hold("distinct");
        Ok(())
    }

    pub(in crate::exec) fn insert(&mut self, v: &Value) -> Result<()> {
        if v.is_null() {
            return Ok(());
        }
        enum Step {
            Done,
            InsertedInt(i64),
            InsertedKey(GroupKey),
            Promote,
        }
        let step = match &mut self.kind {
            DistinctKind::Ints(set) => {
                if let Some(i) = exact_int(v) {
                    if set.insert(i) {
                        Step::InsertedInt(i)
                    } else {
                        Step::Done
                    }
                } else {
                    Step::Promote
                }
            }
            DistinctKind::Keys(keys) => {
                let key = GroupKey::new(v);
                if keys.insert(key.clone()) {
                    Step::InsertedKey(key)
                } else {
                    Step::Done
                }
            }
        };
        match step {
            Step::Done => Ok(()),
            Step::Promote => self.promote(v),
            Step::InsertedInt(i) => {
                let old = match &self.kind {
                    DistinctKind::Ints(set) => set.len().saturating_sub(1),
                    DistinctKind::Keys(_) => 0,
                };
                self.charge_delta(crate::workspace::int_insert_delta(old))
                    .inspect_err(|_| {
                        if let DistinctKind::Ints(set) = &mut self.kind {
                            set.remove(&i);
                        }
                    })
            }
            Step::InsertedKey(key) => {
                let old = match &self.kind {
                    DistinctKind::Keys(set) => set.len().saturating_sub(1),
                    DistinctKind::Ints(_) => 0,
                };
                let delta = if cypherlite_core::tokens::workspace_accounting() {
                    crate::workspace::group_key_insert_delta(old, &key)
                } else {
                    0
                };
                self.charge_delta(delta).inspect_err(|_| {
                    if let DistinctKind::Keys(keys) = &mut self.kind {
                        keys.remove(&key);
                    }
                })
            }
        }
    }

    /// Move the integer set onto keys, carrying `v`. Only `insert`'s
    /// integer arm calls this. The charge stays in place and is resized
    /// onto the key set, so the reservation is not dropped and retaken.
    fn promote(&mut self, v: &Value) -> Result<()> {
        let DistinctKind::Ints(ints) = &self.kind else {
            return Ok(());
        };
        let mut keys = HashSet::with_capacity(ints.len() + 1);
        keys.extend(ints.iter().map(|i| GroupKey::new(&Value::Int(*i))));
        keys.insert(GroupKey::new(v));
        self.kind = DistinctKind::Keys(keys);
        self.sync()
    }

    pub(in crate::exec) fn len(&self) -> i64 {
        match &self.kind {
            DistinctKind::Ints(set) => set.len() as i64,
            DistinctKind::Keys(set) => set.len() as i64,
        }
    }
}
