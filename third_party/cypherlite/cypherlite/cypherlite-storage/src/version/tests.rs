use super::report::with_version_context;
use super::*;
use cypherlite_core::Error;

#[cfg(parquet_codec)]
#[test]
fn manifest_classification() {
    assert_eq!(manifest::classify(1, false), Classification::Legacy(1));
    assert_eq!(manifest::classify(2, false), Classification::Legacy(2));
    // The format follows the content (ADR-0120 §8): only the pair the
    // writer emits is current.
    assert_eq!(
        manifest::classify(manifest::CURRENT_VERSION, false),
        Classification::Current
    );
    assert_eq!(
        manifest::classify(manifest::ONTOLOGY_VERSION, true),
        Classification::Current
    );
    // Readable but content-mismatched: migrate-on-open normalizes to the
    // content's version.
    assert_eq!(
        manifest::classify(manifest::CURRENT_VERSION, true),
        Classification::Legacy(manifest::CURRENT_VERSION)
    );
    assert_eq!(
        manifest::classify(manifest::ONTOLOGY_VERSION, false),
        Classification::Legacy(manifest::ONTOLOGY_VERSION)
    );
    assert_eq!(manifest::classify(5, false), Classification::Unreadable(5));
    assert_eq!(manifest::classify(5, true), Classification::Unreadable(5));
    assert_eq!(
        manifest::classify(99, false),
        Classification::Unreadable(99)
    );
}

#[test]
fn value_encoding_classification_normalizes_the_absent_marker_once() {
    // Absent means encoding 1 (ADR-0127 Decision 1): readable, legacy.
    assert_eq!(
        value_encoding::classify(None, false),
        Classification::Legacy(value_encoding::ABSENT)
    );
    assert_eq!(
        value_encoding::classify(Some(value_encoding::CURRENT_VERSION), false),
        Classification::Current
    );
    // The format follows the content (ADR-0120 §8): encoding 3 is current
    // only with the inline ontology, encoding 2 only without it.
    assert_eq!(
        value_encoding::classify(Some(value_encoding::ONTOLOGY_VERSION), true),
        Classification::Current
    );
    assert_eq!(
        value_encoding::classify(Some(value_encoding::ONTOLOGY_VERSION), false),
        Classification::Legacy(value_encoding::ONTOLOGY_VERSION)
    );
    assert_eq!(
        value_encoding::classify(Some(value_encoding::CURRENT_VERSION), true),
        Classification::Legacy(value_encoding::CURRENT_VERSION)
    );
    assert_eq!(
        value_encoding::classify(Some(4), false),
        Classification::Unreadable(4)
    );
    assert_eq!(
        value_encoding::classify(Some(4), true),
        Classification::Unreadable(4)
    );
}

#[test]
fn value_encoding_gate_returns_the_stores_actual_version_when_current() {
    // A Current-classified store comes back with the version its content
    // wrote, not the no-ontology default: encoding 3 stays 3 (ADR-0120 §8).
    let (version, marker) =
        value_encoding::gate(Some(value_encoding::ONTOLOGY_VERSION), true).unwrap();
    assert_eq!(version, value_encoding::ONTOLOGY_VERSION);
    assert_eq!(marker, SnapshotMarker::Current);

    let (version, marker) =
        value_encoding::gate(Some(value_encoding::CURRENT_VERSION), false).unwrap();
    assert_eq!(version, value_encoding::CURRENT_VERSION);
    assert_eq!(marker, SnapshotMarker::Current);
}

#[test]
fn wal_magic_classification() {
    assert_eq!(wal::classify(wal::CURRENT_MAGIC), Classification::Current);
    assert!(wal::is_readable(wal::CURRENT_MAGIC));
    // ADR-0120 §4: `MDBWAL02` (segments written while the store holds an
    // ontology) is the second readable magic.
    assert_eq!(
        wal::classify(wal::ONTOLOGY_MAGIC),
        Classification::Legacy(*wal::ONTOLOGY_MAGIC)
    );
    assert!(wal::is_readable(wal::ONTOLOGY_MAGIC));
    // `MDBWAL03` is the still-future magic that fails closed.
    assert_eq!(
        wal::classify(b"MDBWAL03"),
        Classification::Unreadable(*b"MDBWAL03")
    );
    assert!(!wal::is_readable(b"MDBWAL03"));
}

#[test]
fn bundle_classification_checks_magic_and_version_as_a_pair() {
    assert_eq!(
        bundle::classify(bundle::CURRENT_MAGIC, bundle::CURRENT_VERSION),
        Classification::Current
    );
    // A newer VERSION under the current magic is unreadable.
    assert_eq!(
        bundle::classify(bundle::CURRENT_MAGIC, 2),
        Classification::Unreadable((*bundle::CURRENT_MAGIC, 2))
    );
    // A foreign magic is unreadable even at a known VERSION.
    assert_eq!(
        bundle::classify(b"MDBCLB02", 1),
        Classification::Unreadable((*b"MDBCLB02", 1))
    );
}

#[test]
fn annotate_legacy_failure_names_the_version_only_for_legacy_markers() {
    let value_encoding = annotate_legacy_failure(
        Error::storage("failed to parse graph JSON: corrupt"),
        SnapshotMarker::Legacy(Subsystem::ValueEncoding, 1),
    );
    assert_eq!(
        value_encoding.to_string(),
        "storage error: failed to parse graph JSON: corrupt (store snapshot is at value_encoding 1)"
    );

    #[cfg(parquet_codec)]
    {
        let manifest = annotate_legacy_failure(
            Error::io("missing shard"),
            SnapshotMarker::Legacy(Subsystem::Manifest, 2),
        );
        assert_eq!(
            manifest.to_string(),
            "io error: missing shard (store snapshot is at manifest version 2)"
        );
    }

    // A current store keeps its error verbatim.
    let current = annotate_legacy_failure(Error::storage("boom"), SnapshotMarker::Current);
    assert_eq!(current, Error::storage("boom"));
}

#[test]
fn migration_report_records_was_at_version_now_current() {
    let mut report = MigrationReport::default();
    assert!(report.is_empty());
    assert_eq!(report.to_string(), "store is at current format");
    report.record(Subsystem::ValueEncoding, 1);
    assert_eq!(report.migrations(), &[(Subsystem::ValueEncoding, 1)]);
    assert_eq!(report.to_string(), "value_encoding 1→2");

    #[cfg(parquet_codec)]
    {
        report.record(Subsystem::Manifest, 1);
        assert_eq!(
            report.migrations(),
            &[(Subsystem::ValueEncoding, 1), (Subsystem::Manifest, 1)]
        );
        assert_eq!(report.to_string(), "value_encoding 1→2, manifest 1→3");
    }
}

#[test]
fn version_context_preserves_the_error_variant_without_a_doubled_prefix() {
    let storage = with_version_context(
        Error::storage("failed to parse graph JSON: corrupt"),
        format_args!("store snapshot is at value_encoding 1"),
    );
    assert!(matches!(storage, Error::Storage(_)));
    let text = storage.to_string();
    assert_eq!(
        text,
        "storage error: failed to parse graph JSON: corrupt (store snapshot is at value_encoding 1)"
    );
    assert_eq!(text.matches("storage error:").count(), 1, "{text}");

    let io = with_version_context(
        Error::io("missing shard"),
        format_args!("manifest version 2"),
    );
    assert!(matches!(io, Error::Io(_)));
    assert_eq!(
        io.to_string(),
        "io error: missing shard (manifest version 2)"
    );

    // Variants without a free-text payload pass through untouched.
    let conflict = with_version_context(Error::Conflict, format_args!("manifest version 2"));
    assert_eq!(conflict, Error::Conflict);
}
