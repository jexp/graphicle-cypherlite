//! End-to-end behavior suites through the public API (`Database` /
//! `Connection`), split by concern. Machinery-internal tests live next to
//! their modules (`binding`, `commit`, `checkpoint`, `procedures`).

mod catalog;
mod cdc;
mod follow;
mod label_stats;
mod memory;
mod namespace;
mod persist;
mod persisted_sentinel;
mod point;
mod preview;
mod processing_limits;
mod queries;
mod read_admission;
mod resize;
mod shortest_path;
mod stack;
mod txn;

/// Codec expectations in these suites are keyed on this crate's own
/// `parquet` feature, so a codec leaking into a lean build fails here.
#[test]
fn parquet_codec_is_compiled_exactly_with_the_parquet_feature() {
    assert_eq!(
        crate::SnapshotCodecKind::Parquet.is_compiled(),
        cfg!(feature = "parquet")
    );
}
