//! The executor's instrumentation cluster (LABS-448's extraction, the
//! one home the store-read counters, the walk-start slot and the phase
//! clocks share): every PROFILE counter and stamp behind the plan
//! table's kinds, the walk's chosen start, and the seek/walk phase
//! times — re-exported through `exec` so the walkers' `super::` paths
//! keep working.

use std::cell::Cell;

use web_time::Instant;

thread_local! {
    pub(crate) static HOP_EXPANDS: Cell<u64> = const { Cell::new(0) };
    pub(crate) static NEIGHBOR_WALKS: Cell<u64> = const { Cell::new(0) };
    /// The node-store reads: one per candidate a node match examined
    /// (the all-nodes scan, the label scan, the index seek) — the scan
    /// work `HOP_EXPANDS`/`NEIGHBOR_WALKS` never see, because they count
    /// relationship traversal only (LABS-375).
    pub(crate) static NODE_READS: Cell<u64> = const { Cell::new(0) };
}

/// Count one relationship hop expansion (LABS-375: the three store-access
/// counters share one increment idiom, never an inline spelling beside a
/// helper).
pub(crate) fn count_hop_expand() {
    HOP_EXPANDS.with(|c| c.set(c.get() + 1));
}

/// Count one relationship the walk traversed (see [`count_hop_expand`]) —
/// the one counting point every matcher uses: the relationship it actually
/// walked, never a candidate it filtered out.
pub(crate) fn count_neighbor_walk() {
    NEIGHBOR_WALKS.with(|c| c.set(c.get() + 1));
}

/// Count `count` node-store reads (LABS-375): the candidates a node match
/// examined — the scan/seek side of the store-access total. Batched
/// because the one funnel (`node_candidates`) yields the whole candidate
/// list at once, and every node-match path reads it.
pub(crate) fn count_node_reads(count: u64) {
    NODE_READS.with(|c| c.set(c.get() + count));
}

/// The store-read mix right now (LABS-369): the three counters the one
/// `Db Hits` total blends, read at clause boundaries so a per-clause delta
/// costs three thread-local reads, never a per-row one.
pub(crate) fn store_reads() -> crate::ctx::StoreReads {
    crate::ctx::StoreReads {
        hops: HOP_EXPANDS.with(Cell::get),
        neighbor_walks: NEIGHBOR_WALKS.with(Cell::get),
        node_reads: NODE_READS.with(Cell::get),
    }
}

thread_local! {
    /// The start the current clause's walk chose (LABS-369), recorded once
    /// per walk by the selection site and taken at the clause's metric
    /// tick. `None` until a walk records one.
    static WALK_START: Cell<Option<crate::ctx::WalkStart>> = const { Cell::new(None) };
    /// Whether a PROFILE statement's clause arm is running (LABS-448) —
    /// the phase stamps read this before touching the clock, so a
    /// non-PROFILEd statement pays the thread-local read and nothing
    /// else. Armed by the clause's metric tick, disarmed by its drop.
    pub(crate) static PHASE_PROFILING: Cell<bool> = const { Cell::new(false) };
    /// The inclusive seek-phase sum (LABS-448): the elapsed time
    /// resolving and checking node candidates — the store work behind the
    /// `Nodes` kind. Inclusive like the table's `Total` column: a walk
    /// that triggers nested seeks counts both.
    pub(crate) static SEEK_NS: Cell<u128> = const { Cell::new(0) };
    /// The inclusive walk-phase sum (LABS-448): the elapsed time following
    /// relationships — the store work behind the `Rels`/`Degree` kinds.
    pub(crate) static WALK_NS: Cell<u128> = const { Cell::new(0) };
}

/// Arm the phase stamps for a PROFILE clause arm (LABS-448) — the metric
/// tick's first act, undone by its drop, so the stamps never run outside a
/// profiled statement.
pub(crate) fn arm_phase_profiling() {
    PHASE_PROFILING.with(|c| c.set(true));
}

/// Disarm the phase stamps (LABS-448) — the metric tick's last act.
pub(crate) fn disarm_phase_profiling() {
    PHASE_PROFILING.with(|c| c.set(false));
}

/// The phase clock's start for one stamp (LABS-448): taken only while a
/// PROFILE statement's arm runs — `None` on a non-PROFILEd statement, so
/// the off-path cost is this one thread-local read, never an `Instant`.
pub(crate) fn phase_stamp() -> Option<Instant> {
    PHASE_PROFILING.with(Cell::get).then(web_time::Instant::now)
}

/// Count one seek-phase duration (LABS-448): the elapsed time resolving
/// node candidates — the list a scan, label match or index seek produced.
/// The per-candidate admission checks stay unattributed arm time (the
/// arm's `time_ns` covers what the phases do not). A `None` stamp (a
/// non-PROFILEd statement) counts nothing.
pub(crate) fn count_seek_phase(stamp: Option<Instant>) {
    if let Some(start) = stamp {
        SEEK_NS.with(|c| c.set(c.get() + start.elapsed().as_nanos()));
    }
}

/// Count one walk-phase duration (LABS-448): the elapsed time following
/// relationships — the traversal the seek's admitted candidates feed. A
/// `None` stamp counts nothing.
pub(crate) fn count_walk_phase(stamp: Option<Instant>) {
    if let Some(start) = stamp {
        WALK_NS.with(|c| c.set(c.get() + start.elapsed().as_nanos()));
    }
}

/// One seek-phase span around a pattern node's candidate resolution
/// (LABS-448): the one wrapper every walker resolves its candidates
/// through, so the seek stamp lives beside the funnel that bumps the
/// `Nodes` counter — never a re-stamped copy per walker. `None` stamps
/// (non-PROFILEd statements) resolve unchanged.
pub(crate) fn phase_seek<T>(
    resolve: impl FnOnce() -> cypherlite_core::Result<T>,
) -> cypherlite_core::Result<T> {
    let stamp = phase_stamp();
    let out = resolve()?;
    count_seek_phase(stamp);
    Ok(out)
}

/// One walk-phase span around a relationship traversal (LABS-448): the
/// symmetric spelling of [`phase_seek`] — the stamp lives in the helper,
/// so the walker sites never hand-roll the stamp→run→count pairing, and
/// the walk-phase contract (inclusive of nested phases) is stated once,
/// here. `None` stamps resolve unchanged on non-PROFILEd statements.
pub(crate) fn phase_walk<T>(
    walk: impl FnOnce() -> cypherlite_core::Result<T>,
) -> cypherlite_core::Result<T> {
    let stamp = phase_stamp();
    let out = walk()?;
    count_walk_phase(stamp);
    Ok(out)
}

/// The phase-time sums right now (LABS-448), snapshotted at the clause
/// tick the way [`store_reads`] is: the delta across the arm is the
/// clause's phase times, two thread-local reads per clause.
pub(crate) fn phase_times() -> crate::ctx::PhaseTimes {
    crate::ctx::PhaseTimes {
        seek_ns: SEEK_NS.with(Cell::get),
        walk_ns: WALK_NS.with(Cell::get),
    }
}

/// Record how the walk starting now chose its element (LABS-369). The
/// clause's FIRST walk wins: a clause whose start varies per outer row
/// (the bound pivot depends on the row's binding) reports its
/// representative first start rather than the last row's.
pub(crate) fn record_walk_start(start: crate::ctx::WalkStart) {
    WALK_START.with(|slot| {
        if slot.get().is_none() {
            slot.set(Some(start));
        }
    });
}

/// Take the recorded walk start, clearing it: the metric tick reads it
/// once per clause, so a stale start never leaks into the next clause.
pub(crate) fn take_walk_start() -> Option<crate::ctx::WalkStart> {
    WALK_START.with(|slot| slot.take())
}

#[cfg(test)]
pub(crate) fn take_hop_expands() -> u64 {
    HOP_EXPANDS.with(|c| c.replace(0))
}

#[cfg(test)]
pub(crate) fn take_neighbor_walks() -> u64 {
    NEIGHBOR_WALKS.with(|c| c.replace(0))
}

#[cfg(test)]
pub(crate) fn take_node_reads() -> u64 {
    NODE_READS.with(|c| c.replace(0))
}
