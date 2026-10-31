//! `MATCH`-clause shortest paths on breadth-first search (ADR-0186).
//!
//! A routed selector (`shortestPath`, `allShortestPaths`, `ANY SHORTEST`,
//! `SHORTEST 1`, `SHORTEST 1 GROUP`, `ALL SHORTEST`) over two node patterns
//! and one variable-length relationship is answered per (start, end)
//! partition by BFS instead of enumerate-then-select. `match_gpm_path`
//! tries the route first; [`route_shape`] is the single guard, and every
//! pattern it rejects falls through to enumeration unchanged. The searches
//! come from [`crate::shortest`] (ADR-0189). Rows are bound the way the GPM
//! walker binds them; `MATCH ... WHERE` still filters them afterwards.

use std::collections::HashSet;

use cypherlite_core::{RelId, Result};
use cypherlite_ir::{MatchMode, PathPattern};

use crate::ctx::ExecCtx;
use crate::eval::Row;

mod guard;
mod route;

use guard::route_shape;
use route::Route;

#[cfg(test)]
mod bounded_tests;
#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod parity_tests;
#[cfg(test)]
mod reject_tests;
#[cfg(test)]
mod route_tests;

use crate::exec::expand::Matched;

#[cfg(test)]
thread_local! {
    static FORCE_GPM: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ROUTE_RUNS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Count one (pattern, input row) evaluation the route accepted. The
/// counter exists only in tests. Unlike the expansion counter it also moves
/// when a search needs no expansion (the zero-length path of
/// `(a)-[*0..n]-(a)`).
#[inline(always)]
fn note_route_run() {
    #[cfg(test)]
    ROUTE_RUNS.with(|c| c.set(c.get() + 1));
}

/// Take and reset this thread's count of route runs.
#[cfg(test)]
pub(crate) fn take_route_runs() -> u64 {
    ROUTE_RUNS.with(|c| c.replace(0))
}

/// Run `f` with every pattern the route would take sent to `match_gpm_path`
/// instead, for route-versus-GPM parity tests. Restores the previous
/// setting on return or unwind.
#[cfg(test)]
fn with_forced_gpm<T>(f: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            FORCE_GPM.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(FORCE_GPM.with(|c| c.replace(true)));
    f()
}

/// Match `path` on the BFS route, or `Ok(None)` when [`route_shape`] rejects
/// it. Each row's used set is its trail's relationships, as GPM's would be:
/// the guard admits only an empty incoming set.
pub(super) fn match_shortest_route(
    ctx: ExecCtx<'_>,
    path: &PathPattern,
    row: &Row,
    used: &HashSet<RelId>,
    match_mode: MatchMode,
) -> Result<Option<Matched>> {
    #[cfg(test)]
    if FORCE_GPM.with(|c| c.get()) {
        return Ok(None);
    }
    let Some(shape) = route_shape(path, match_mode, used, row) else {
        return Ok(None);
    };
    note_route_run();
    Route::new(ctx, path, row, shape).run().map(Some)
}
