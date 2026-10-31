//! The runtime half of batched bulk writes (ADR-0209): the snapshotted
//! target space, the cursor, the budgeted loop, and [`BulkWrite`]'s
//! phase-1/phase-2 methods. Plan analysis lives in [`super`].

use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{NodeId, RelId, Result, Value};
use cypherlite_ir::Direction;

use crate::eval::Row;
use crate::graph::Graph;
use crate::overlay::ExecGraph;

use super::super::create::create_patterns;
use super::super::write::{apply_resolved_removes, apply_sets, resolve_removes};
use super::{BulkKind, BulkWrite, FactorSource};

/// Cancellation-check cadence of the budgeted loop: the token is polled
/// once per this many steps.
const CANCEL_CHECK_STRIDE: u64 = 1024;

/// One deduplicated `DELETE` target. The node/relationship distinction
/// is the variant, keeping the two id namespaces apart; a node's `work`
/// estimate is 1 + incident-degree, a relationship's is uniformly 1.
#[derive(Debug, Clone, Copy)]
enum BulkEntity {
    Node { id: NodeId, work: u64 },
    Rel { id: RelId },
}

impl BulkEntity {
    fn work(&self) -> u64 {
        match self {
            Self::Node { work, .. } => *work,
            Self::Rel { .. } => 1,
        }
    }
}

/// The snapshotted target space plus the write payload it drives. Each
/// variant carries everything its execution arm needs, so there is no
/// kind/targets pairing to get wrong.
#[derive(Debug)]
enum TargetsInner {
    /// `DELETE`: deduplicated entities in id order (nodes first).
    Delete {
        detach: bool,
        entities: Vec<BulkEntity>,
    },
    /// `SET`: one sorted id vector per factor; the row space is their
    /// cartesian product (never materialized).
    Set {
        items: Vec<cypherlite_ir::SetItem>,
        factors: Vec<Vec<i64>>,
    },
    /// `REMOVE`: same row space as `SET`.
    Remove {
        items: Vec<cypherlite_ir::RemoveItem>,
        factors: Vec<Vec<i64>>,
    },
    /// `CREATE`: same row space as `SET`.
    Create {
        patterns: Vec<cypherlite_ir::PathPattern>,
        factors: Vec<Vec<i64>>,
    },
}

/// The snapshotted target identity space of a [`BulkWrite`] (phase 1).
/// Opaque: the only public fact is [`BulkTargets::total_work`].
#[derive(Debug)]
pub struct BulkTargets(TargetsInner);

impl BulkTargets {
    /// The estimated touched-entity count used against the batching
    /// threshold: entity count plus incident relationships for `DELETE`,
    /// the factor product for the row-driven writes.
    pub fn total_work(&self) -> u64 {
        match &self.0 {
            TargetsInner::Delete { entities, .. } => entities
                .iter()
                .map(BulkEntity::work)
                .fold(0u64, u64::saturating_add),
            TargetsInner::Set { factors, .. }
            | TargetsInner::Remove { factors, .. }
            | TargetsInner::Create { factors, .. } => factors
                .iter()
                .map(|ids| ids.len() as u64)
                .fold(1u64, u64::saturating_mul),
        }
    }
}

/// Cursor into the target space: the next entity index (`DELETE`) or
/// combination index (row-driven writes) to process.
#[derive(Debug, Default)]
pub struct BulkCursor {
    next: u64,
}

/// The budgeted loop both phase-2 arms share: cancel-check every
/// [`CANCEL_CHECK_STRIDE`] steps, advance the cursor, accumulate the
/// work `step` reports, and stop once `budget` work units are spent or
/// the `total`-sized space is exhausted. Returns `true` when the space
/// is exhausted.
fn run_budgeted(
    view: &ExecGraph<'_>,
    cursor: &mut BulkCursor,
    total: u64,
    budget: u64,
    mut step: impl FnMut(u64) -> Result<u64>,
) -> Result<bool> {
    let mut spent = 0u64;
    while cursor.next < total {
        if cursor.next % CANCEL_CHECK_STRIDE == 0 {
            view.check_cancelled()?;
        }
        let work = step(cursor.next)?;
        cursor.next += 1;
        spent = spent.saturating_add(work);
        if spent >= budget {
            break;
        }
    }
    Ok(cursor.next >= total)
}

impl BulkWrite {
    /// Phase 1: snapshot the matched entity id space at `snapshot_ts`.
    /// Returns `Ok(None)` when the catalog holds any constraint: batched
    /// commits validate per batch, so constrained catalogs keep the
    /// unbatched atomic path (ADR-0209 §1). A non-`DETACH` node delete
    /// preflights `DeleteConnectedNode` across every target here, before
    /// the first batch commits.
    pub fn collect_targets(&self, graph: &Graph, snapshot_ts: u64) -> Result<Option<BulkTargets>> {
        if !graph.named_constraints().is_empty() {
            return Ok(None);
        }
        let view = ExecGraph::read_at(graph, snapshot_ts);
        let mut factors = Vec::with_capacity(self.factors.len());
        for factor in &self.factors {
            let ids = match &factor.source {
                // Labeled/typed factors go through the interned-id
                // probes; only the untyped/unlabeled factors pay a full
                // sweep.
                FactorSource::Nodes { labels } if !labels.is_empty() => {
                    let mut sets: Vec<Vec<NodeId>> = labels
                        .iter()
                        .map(|label| graph.node_ids_with_label_at(label, snapshot_ts))
                        .collect();
                    sets.sort_by_key(Vec::len);
                    let (smallest, rest) = sets.split_first().expect("non-empty labels");
                    let rest: Vec<HashSet<NodeId>> = rest
                        .iter()
                        .map(|ids| ids.iter().copied().collect())
                        .collect();
                    smallest
                        .iter()
                        .copied()
                        .filter(|id| rest.iter().all(|set| set.contains(id)))
                        .collect()
                }
                FactorSource::Nodes { .. } => view.all_node_ids(),
                FactorSource::Rels { types } if !types.is_empty() => {
                    let mut ids: Vec<RelId> = types
                        .iter()
                        .flat_map(|rel_type| graph.rel_ids_of_type_at(rel_type, snapshot_ts))
                        .collect();
                    ids.sort_unstable();
                    ids.dedup();
                    ids
                }
                FactorSource::Rels { .. } => view.all_rel_ids(),
            };
            factors.push(ids);
        }
        let inner = match &self.kind {
            BulkKind::Delete { detach, targets } => {
                self.collect_delete_targets(&view, *detach, targets, &factors)?
            }
            BulkKind::Set { items } => TargetsInner::Set {
                items: items.clone(),
                factors,
            },
            BulkKind::Remove { items } => TargetsInner::Remove {
                items: items.clone(),
                factors,
            },
            BulkKind::Create { patterns } => TargetsInner::Create {
                patterns: patterns.clone(),
                factors,
            },
        };
        Ok(Some(BulkTargets(inner)))
    }

    /// The `DELETE` target space: gather the ids of the factors the
    /// `DELETE` names (deduplicated across the possibly cross-product
    /// row space — deleting an entity is idempotent, so each id is
    /// processed once), preflight `DeleteConnectedNode` for a
    /// non-`DETACH` delete, and estimate each entity's work.
    fn collect_delete_targets(
        &self,
        view: &ExecGraph<'_>,
        detach: bool,
        targets: &HashSet<String>,
        factor_ids: &[Vec<i64>],
    ) -> Result<TargetsInner> {
        let mut node_ids: Vec<NodeId> = Vec::new();
        let mut rel_ids: Vec<RelId> = Vec::new();
        for (factor, ids) in self.factors.iter().zip(factor_ids) {
            if !targets.contains(&factor.var) {
                continue;
            }
            match factor.source {
                FactorSource::Nodes { .. } => node_ids.extend_from_slice(ids),
                FactorSource::Rels { .. } => rel_ids.extend_from_slice(ids),
            }
        }
        node_ids.sort_unstable();
        node_ids.dedup();
        rel_ids.sort_unstable();
        rel_ids.dedup();
        if !detach {
            // Preflight the DeleteConnectedNode error before the first
            // batch commits, keeping the statement atomic for this error
            // while no concurrent writer adds a relationship after this
            // snapshot.
            for &id in &node_ids {
                let connected = view
                    .try_find_neighbor(id, Direction::Both, &[], |_| Ok::<bool, ()>(true))
                    .is_ok_and(|hit| hit.is_some());
                if connected {
                    return Err(crate::graph::delete_connected_node_error());
                }
            }
        }
        let mut entities = Vec::with_capacity(node_ids.len() + rel_ids.len());
        for id in node_ids {
            let work = if detach {
                1u64.saturating_add(view.adj_len(id, Direction::Both) as u64)
            } else {
                1
            };
            entities.push(BulkEntity::Node { id, work });
        }
        entities.extend(rel_ids.into_iter().map(|id| BulkEntity::Rel { id }));
        Ok(TargetsInner::Delete { detach, entities })
    }

    /// Phase 2: apply up to `budget` work units starting at `cursor` on
    /// the caller's write view (which carries the cancellation token).
    /// Returns `true` when the target space is exhausted. The caller
    /// commits the view's scratch per chunk.
    pub fn execute_chunk(
        &self,
        view: &ExecGraph<'_>,
        targets: &BulkTargets,
        cursor: &mut BulkCursor,
        budget: u64,
        params: &BTreeMap<String, Value>,
    ) -> Result<bool> {
        match &targets.0 {
            TargetsInner::Delete { detach, entities } => {
                let detach = *detach;
                run_budgeted(view, cursor, entities.len() as u64, budget, |i| {
                    let entity = entities[i as usize];
                    match entity {
                        BulkEntity::Node { id, .. } => view.delete_node(id, detach)?,
                        BulkEntity::Rel { id } => view.delete_rel(id),
                    }
                    Ok(entity.work())
                })
            }
            TargetsInner::Set { items, factors } => {
                self.run_row_space(view, targets, factors, cursor, budget, |view, row| {
                    apply_sets(view, row, items, params, self.cypher_version)
                })
            }
            TargetsInner::Remove { items, factors } => {
                self.run_row_space(view, targets, factors, cursor, budget, |view, row| {
                    let resolved = resolve_removes(view, row, items, params, self.cypher_version)?;
                    apply_resolved_removes(view, resolved)
                })
            }
            TargetsInner::Create { patterns, factors } => {
                self.run_row_space(view, targets, factors, cursor, budget, |view, row| {
                    create_patterns(view, row, patterns, params, self.cypher_version)
                })
            }
        }
    }

    /// The scaffold every row-driven write flavor shares: budgeted walk of
    /// the factor product, one executed combination per step; only the
    /// per-row write differs.
    fn run_row_space(
        &self,
        view: &ExecGraph<'_>,
        targets: &BulkTargets,
        factors: &[Vec<i64>],
        cursor: &mut BulkCursor,
        budget: u64,
        write: impl Fn(&ExecGraph<'_>, &mut Row) -> Result<()>,
    ) -> Result<bool> {
        run_budgeted(view, cursor, targets.total_work(), budget, |combo| {
            self.execute_combo(view, factors, combo, &write)?;
            Ok(1)
        })
    }

    /// One combination of the factor product: decode the flat index into
    /// per-factor ids (last factor varies fastest, matching the engine's
    /// nested-match row order), bind every factor variable to the entity's
    /// fresh value at this chunk's snapshot, and run the write clause
    /// through the same per-row logic (`write`) the unbatched dispatch
    /// uses.
    fn execute_combo(
        &self,
        view: &ExecGraph<'_>,
        factors: &[Vec<i64>],
        combo: u64,
        write: impl FnOnce(&ExecGraph<'_>, &mut Row) -> Result<()>,
    ) -> Result<()> {
        let mut row = Row::new();
        let mut rest = combo;
        for (factor, ids) in self.factors.iter().zip(factors).rev() {
            let id = ids[(rest % ids.len() as u64) as usize];
            rest /= ids.len() as u64;
            let value = match factor.source {
                FactorSource::Nodes { .. } => view.node(id).map(Value::from_node),
                FactorSource::Rels { .. } => view.rel(id).map(Value::from_relationship),
            };
            // A concurrently deleted entity voids its combinations: the
            // optimistic conflict check already rejected overlapping
            // writes where it can; skipping keeps the target snapshot's
            // surviving space exact.
            let Some(value) = value else {
                return Ok(());
            };
            row.insert(factor.var.clone(), value);
        }
        write(view, &mut row)
    }
}
