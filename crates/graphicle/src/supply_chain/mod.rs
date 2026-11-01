//! The package facts of a project's own lockfile (supply-chain plan §2 and
//! §4 P1): `Cargo.lock` read offline during sync and written as
//! `(:Package)`, `(:PackageVersion)`, `REQUIRES` and `DESCRIBES` edges.
//!
//! Nothing here touches the network. deps.dev and OSV facts (licenses,
//! advisories, release dates) are later nodes' business; the lockfile
//! carries none of them, so they stay absent until those nodes fill them.
//!
//! [`lock::ingest`] is the sync's body for this file: it runs after the
//! folder is read, reports its counts under the sync's `cargo_lock` key, and
//! a failure is reported there, never as a failed sync.

pub mod lock;
#[cfg(test)]
mod tests;
