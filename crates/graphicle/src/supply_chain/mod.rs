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
//! [`lock::ingest`] is the sync's body for the lockfile: it runs after the
//! folder is read, reports its counts under the sync's `cargo_lock` key, and
//! a failure is reported there, never as a failed sync.
//!
//! [`osv`] is the advisory half: one bulk query per lockfile digest, queued
//! by the sync after the lockfile is ingested and drained as an `osv` job.
//!
//! [`batches`] is the writer both lockfile and advisory facts share.

use crate::Result;
use crate::store_graph::{self, GraphStore};
use serde_json::{Value, json};

pub mod deps_dev;
pub mod http_cache;
pub mod lock;
pub mod osv;
#[cfg(test)]
pub(crate) mod osv_tests;
#[cfg(test)]
mod tests;

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
