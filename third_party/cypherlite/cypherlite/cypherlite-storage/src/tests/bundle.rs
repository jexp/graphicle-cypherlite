//! `.cl` single-file bundles: a `.cl` path (any case, also under
//! `file://`) opens a bundle that holds the snapshot and the WAL in one
//! file, never a directory. A `.cl` directory, a `wal_dir` and a file that
//! is not a bundle are refused; any other path stays a native directory.

use super::*;

#[test]
fn cl_suffix_opens_bundle_not_directory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.cl");
    let uri = path.to_str().unwrap();
    let (g, mut wal) = person_ada();
    let target = parse_uri(uri).unwrap();
    assert!(target.filesystem_dir().is_none());
    checkpoint_with_lsn(&target, &g, wal.lsn, None).unwrap();
    let cut = wal.rotate().unwrap();
    wal.recycle(cut).unwrap();
    drop(wal);
    drop(target);
    assert!(path.is_file());
    assert!(!dir.path().join("wal").exists());
    let target = parse_uri(uri).unwrap();
    let loaded = load(&target).unwrap();
    assert_eq!(loaded.live_node_count(), 1);
    drop(target);
    reset_uri(uri, None).unwrap();
    assert!(!path.exists());
    reset_uri(uri, None).unwrap();
}

#[test]
fn bundle_checkpoint_with_wal_on_same_vfs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.cl");
    let uri = path.to_str().unwrap();
    let (g, mem_wal) = person_ada();
    let target = parse_uri(uri).unwrap();
    let (wv, prefix) = target.wal_vfs();
    let mut wal = Wal::open(wv, prefix, 0).unwrap();
    for rec in mem_wal.replay(0).unwrap() {
        wal.append_commit(rec.commit_ts, &rec.ops).unwrap();
    }
    let cut = wal.rotate().unwrap();
    checkpoint_with_lsn(&target, &g, wal.lsn, None).unwrap();
    wal.recycle(cut).unwrap();
    wal.append_commit(
        2,
        &[
            cypherlite_executor::RedoOp::CreateNode {
                id: 1,
                labels: vec!["Person".into()].into(),
                properties: Default::default(),
            },
            cypherlite_executor::RedoOp::PullIdentityPut {
                source: "upstream".into(),
                kind: cypherlite_executor::PullKind::Node,
                remote_id: "bob".into(),
                local_id: 1,
            },
        ],
    )
    .unwrap();
    drop(wal);
    drop(target);
    let target = parse_uri(uri).unwrap();
    let (loaded, reopened) = open_store(&target).unwrap();
    assert_eq!(reopened.lsn, 2);
    assert_eq!(loaded.live_node_count(), 2);
    assert_eq!(
        loaded.pull_identity_get("upstream", cypherlite_executor::PullKind::Node, "bob"),
        Some(1)
    );
}

#[test]
fn file_scheme_cl_is_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.CL");
    let uri = format!("file://{}", path.display());
    let target = parse_uri(&uri).unwrap();
    assert!(target.filesystem_dir().is_none());
    assert!(target.vfs().durable());
}

#[test]
fn cl_directory_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.cl");
    std::fs::create_dir(&path).unwrap();
    let err = parse_uri(path.to_str().unwrap())
        .err()
        .expect("directory named .cl");
    assert!(err.to_string().contains("directory"), "{err}");
}

#[test]
fn cl_rejects_wal_dir() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.cl");
    let err = parse_uri_with_wal(path.to_str().unwrap(), Some(dir.path().join("wal")))
        .err()
        .expect("wal_dir rejected");
    assert!(err.to_string().contains("wal_dir"), "{err}");
}

#[test]
fn non_cl_path_stays_native_directory() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let target = parse_uri(uri).unwrap();
    assert_eq!(
        target.filesystem_dir().unwrap(),
        std::fs::canonicalize(dir.path()).unwrap()
    );
}

#[test]
fn garbage_cl_file_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.cl");
    std::fs::write(&path, b"not a bundle").unwrap();
    let err = parse_uri(path.to_str().unwrap())
        .err()
        .expect("garbage .cl");
    assert!(err.to_string().contains("MDBCLB01"), "{err}");
}
