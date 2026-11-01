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

pub mod deps_dev;
pub mod http_cache;
pub mod lock;
pub mod resolve;
#[cfg(test)]
mod tests;
