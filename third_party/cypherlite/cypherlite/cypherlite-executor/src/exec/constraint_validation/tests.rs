//! Incremental constraint validation (ADR-0172) at the executor level: the
//! inspection counter's independence from graph size, the validate-then-mark
//! cursor (ADR-0173), the committed-catalog fingerprint (ADR-0174), the
//! fail-closed parse of a stored constraint, and a differential check
//! against the full-scan validator it replaced, kept below as a test-only
//! oracle. The counter is thread-local, so these tests run
//! statements on the test thread through the `exec::test_support` write
//! helpers rather than through `cypherlite::Connection`.
//!
//! - [`harness`]: shared seeding, the inspection-counter probe, and the DDL
//!   statements every concern uses.
//! - [`scale`]: the inspection-counter performance tests (ADR-0172).
//! - [`oracle`]: the full-scan oracle and the differential corpus.
//! - [`behavior`]: cursor, fingerprint, fail-closed and type-matching
//!   behavior tests.

mod behavior;
mod harness;
mod oracle;
mod scale;
