//! Supply-chain facts about a project's external packages (supply-chain
//! plan §1, §2 and §4 P1/P2).
//!
//! [`lock`] is the offline half: `Cargo.lock` read during sync and written
//! as `(:Package)`, `(:PackageVersion)`, `REQUIRES` and `DESCRIBES` edges.
//! It never touches the network; deps.dev and OSV facts (licenses,
//! advisories, release dates) are not in the lockfile, so they stay absent
//! until the enrichment nodes fill them.
//!
//! [`deps_dev`] is the deps.dev v3 client and the purl decomposition its
//! keys come from. [`http_cache`] is the read-through cache and
//! self-throttle under it. It is HTTP-generic, so the OSV client reuses it
//! under its own namespace. Neither writes to the graph store; the store
//! integration is the supply-chain job's business.
//!
//! [`resolve`] is the seam of plan §3: the Rust imports the code graph
//! holds (`imports_from` edges carrying their use paths) resolve to the
//! packages the lockfile wrote, as `(:Symbol)-[:RESOLVES_TO]->(:Package)`
//! edges, and an import that matches no local path, member or locked crate
//! lands as `:UnresolvedPackage` negative evidence with the reason. The
//! sync runs it after the lock ingest and the projection job after it
//! persists, so whichever fact arrived last re-runs it; it is a full
//! recompute, so neither order leaves it stale.

//! [`lock::ingest`] is the sync's body for the lockfile: it runs after the
//! folder is read, reports its counts under the sync's `cargo_lock` key, and
//! a failure is reported there, never as a failed sync.
//!
//! [`osv`] is the advisory half: one bulk query per lockfile digest, queued
//! by the sync after the lockfile is ingested and drained as an `osv` job.
//!
//! [`on_demand`] is the enrichment half of plan §4 P2: deps.dev is a
//! read-through source, never an eager job. An analysis, a tool or a pack
//! calls it when it needs a license, a Scorecard, a source repository, a
//! deprecation or a manifest requirement; the call serves from the HTTP
//! cache and fetches through the shared throttle only on a miss, and writes
//! nothing to the store. The store grows only through
//! [`on_demand::pin`](on_demand::pin), when an answer must outlive the
//! question: the facts it consumed are written with origin, `known_at`,
//! `as_of` and the analysis reference, so a pinned analysis re-runs against
//! the pinned data. There is no deps.dev job kind — OSV stays the only
//! supply-chain job a sync queues, the plan's one eager exception.
//!
//! [`batches`] is the writer lockfile, advisory and pin facts share.

use crate::Result;
use crate::store_graph::{self, GraphStore};
use serde_json::{Value, json};

pub mod deps_dev;
pub mod http_cache;
pub mod lock;
pub mod on_demand;
#[cfg(test)]
pub(crate) mod on_demand_tests;
pub mod osv;
#[cfg(test)]
pub(crate) mod osv_tests;
pub mod resolve;
#[cfg(test)]
mod tests;
pub mod tools;
#[cfg(test)]
pub(crate) mod tools_tests;

/// Rows per transaction (git_history/writer.rs's `BATCH`).
const BATCH: usize = 1000;

/// Run `query` once per [`BATCH`]-row chunk of `rows`, one transaction per
/// chunk. The query reads its rows as `$rows` and the project as `$repo`.
pub(crate) fn batches(
    graph: &GraphStore,
    repository_id: &str,
    rows: &[Value],
    query: &str,
) -> Result<()> {
    for chunk in rows.chunks(BATCH) {
        graph.transaction(|db| {
            store_graph::write(
                db,
                query,
                store_graph::params(&json!({"repo": repository_id, "rows": chunk})),
            )?;
            Ok(())
        })?;
    }
    Ok(())
}
