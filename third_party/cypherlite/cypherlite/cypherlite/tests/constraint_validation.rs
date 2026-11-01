//! Constraint validation through the public API (ADR-0172): one file per
//! concern — the kind x trigger matrix, transaction duplicates, the closed
//! concurrent duplicate-commit gap (ADR-0439) beside concurrent probes, the
//! raw-WAL replay duplicate, constraints another session commits
//! mid-transaction (ADR-0174), and persistence across a reopen.

#[path = "common/mod.rs"]
mod common;
#[macro_use]
#[path = "constraint_validation/support.rs"]
mod support;

#[path = "constraint_validation/concurrency.rs"]
mod concurrency;
#[path = "constraint_validation/cross_session.rs"]
mod cross_session;
#[path = "constraint_validation/matrix.rs"]
mod matrix;
#[path = "constraint_validation/persistence.rs"]
mod persistence;
#[path = "constraint_validation/replay.rs"]
mod replay;
#[path = "constraint_validation/transactions.rs"]
mod transactions;
