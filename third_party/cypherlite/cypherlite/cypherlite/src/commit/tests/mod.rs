//! Commit-path tests, split by theme: `harness` (shared helpers),
//! `concurrency` (reader/writer races and durability modes), `admission_races`
//! (ADR-0439 scheduler-driven uniqueness races), `group_commit` (batch
//! flushing and safe horizons), and `faults` (injected I/O failures and
//! uncertain-outcome fencing).

mod admission_races;
mod concurrency;
mod faults;
mod group_commit;
mod harness;
