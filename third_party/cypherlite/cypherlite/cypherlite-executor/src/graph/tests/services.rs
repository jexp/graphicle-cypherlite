use super::*;

use cypherlite_core::PreviewFeatures;

use crate::csv::CsvPolicy;

#[test]
fn configure_load_csv_applies_policy_before_sharing() {
    let mut graph = Graph::default();
    let policy = CsvPolicy::new(None, Some(1024)).unwrap();
    graph.configure_load_csv(policy);
    assert_eq!(graph.csv_policy().max_bytes(), 1024);
    // Sharing after configuration propagates the installed policy.
    let mut other = Graph::default();
    other.share_services_from(&graph);
    assert_eq!(other.csv_policy().max_bytes(), 1024);
}

/// The follow reload's bundle hand-off: the checkpoint-loaded databases
/// catalog is installed into the store's surviving bundle while that
/// bundle's configuration (procedure registry, CSV policy, preview set)
/// stays the store's, and the replacement then shares the one `Arc`.
#[test]
fn install_databases_from_carries_only_the_durable_catalog() {
    let mut store = Graph::default();
    store.configure_load_csv(CsvPolicy::new(None, Some(1024)).unwrap());
    store.configure_preview(PreviewFeatures::none());
    store.databases().put_database("neo4j", 25, false);
    store.register_procedure(Procedure {
        name: "test.proc".into(),
        arg_names: Vec::new(),
        arg_types: Vec::new(),
        yield_names: Vec::new(),
        rows: Vec::new(),
    });

    let loaded = Graph::default();
    loaded.databases().put_database("neo4j", 5, false);

    store.install_databases_from(&loaded);
    let mut replacement = Graph::default();
    replacement.share_services_from(&store);

    assert_eq!(replacement.csv_policy().max_bytes(), 1024);
    assert_eq!(replacement.preview(), PreviewFeatures::none());
    assert!(replacement.has_procedure("test.proc"));
    assert_eq!(
        replacement.databases().snapshot()[&Namespace::Neo4j].default_language,
        5,
        "the replacement reads the checkpoint's catalog through the store bundle"
    );
    assert_eq!(
        store.databases().snapshot()[&Namespace::Neo4j].default_language,
        5,
        "the loaded catalog is installed into the one surviving table"
    );
}

/// The StoreServices CSV surface: the default policy denies local files
/// with the default byte limit, `configure_load_csv` installs a validated
/// policy, and a failed `CsvPolicy::new` construction never disturbs the
/// capability already installed (the invalid value is rejected before
/// `configure_load_csv` can run).
#[test]
fn csv_policy_defaults_and_configure_preserves_capability_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut graph = Graph::default();
    assert_eq!(
        graph.csv_policy().max_bytes(),
        crate::csv::DEFAULT_MAX_BYTES
    );
    assert!(graph.csv_policy().root().is_none());
    graph.configure_load_csv(CsvPolicy::new(Some(root.to_path_buf()), Some(5)).unwrap());
    let expected_root = root.canonicalize().unwrap();
    for limit in [0, 67_108_865, u64::MAX] {
        assert!(CsvPolicy::new(Some(root.to_path_buf()), Some(limit)).is_err());
        assert_eq!(graph.csv_policy().root(), Some(expected_root.as_path()));
        assert_eq!(graph.csv_policy().max_bytes(), 5);
    }
    for invalid_root in [root.join("missing"), root.join("rows.csv")] {
        assert!(CsvPolicy::new(Some(invalid_root), None).is_err());
        assert_eq!(graph.csv_policy().root(), Some(expected_root.as_path()));
    }
    graph.configure_load_csv(CsvPolicy::new(None, Some(67_108_864)).unwrap());
    assert_eq!(graph.csv_policy().max_bytes(), 67_108_864);
}

#[test]
#[should_panic(expected = "must be configured before share_services_from")]
fn configure_load_csv_after_share_services_from_fails_loudly() {
    let store = Graph::default();
    let mut graph = Graph::default();
    graph.share_services_from(&store);
    graph.configure_load_csv(CsvPolicy::new(None, Some(1024)).unwrap());
}

/// The preview installer shares [`Graph::services_mut`]'s one pre-share
/// contract with the CSV installer: after a share, re-installing the set
/// would rebuild the `Arc` and detach the graph from the store's bundle.
#[test]
#[should_panic(expected = "must be configured before share_services_from")]
fn configure_preview_after_share_services_from_fails_loudly() {
    let store = Graph::default();
    let mut graph = Graph::default();
    graph.share_services_from(&store);
    graph.configure_preview(PreviewFeatures::none());
}
