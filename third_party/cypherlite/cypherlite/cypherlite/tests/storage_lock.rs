use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use cypherlite::{Database, Error, OpenOptions};
use cypherlite_storage::LOCK_MARKER;
use cypherlite_test_support::uri;

fn locked(result: cypherlite::Result<Database>) {
    assert!(
        matches!(result, Err(Error::Locked(_))),
        "expected a lock contention error"
    );
}

fn child(path: &Path, mode: &str) -> std::process::Output {
    Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("storage_lock_child")
        .arg("--nocapture")
        .env("CYPHERLITE_LOCK_CHILD", mode)
        .env("CYPHERLITE_LOCK_PATH", path)
        .output()
        .unwrap()
}

#[test]
fn storage_lock_child() {
    let Ok(mode) = std::env::var("CYPHERLITE_LOCK_CHILD") else {
        return;
    };
    let path = PathBuf::from(std::env::var_os("CYPHERLITE_LOCK_PATH").unwrap());
    match mode.as_str() {
        "locked" => locked(Database::open(uri(&path))),
        "open" => {
            drop(Database::open(uri(&path)).unwrap());
        }
        "hold" => {
            let _db = Database::open(uri(&path)).unwrap();
            println!("READY");
            let mut byte = [0];
            let _ = std::io::Read::read_exact(&mut std::io::stdin(), &mut byte);
        }
        #[cfg(unix)]
        "modes" | "bundle_modes" => {
            // This test subprocess has no concurrent tests; a permissive
            // umask cannot interfere with another process's mode assertions.
            extern "C" {
                fn umask(mask: u32) -> u32;
            }
            unsafe {
                umask(0);
            }
            let db = Database::open(uri(&path)).unwrap();
            let mut conn = db.connect().unwrap();
            conn.query("CREATE (:Person {name: 'Private'})").unwrap();
            db.register_cdc_slot("reader").unwrap();
            db.checkpoint().unwrap();
            drop((conn, db));
            if mode == "modes" {
                assert_private_tree(&path);
            } else {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        _ => panic!("unknown child mode {mode}"),
    }
}

#[test]
fn same_process_owner_survives_connections_clones_and_checkpoint() {
    for bundle in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(if bundle { "graph.cl" } else { "graph" });
        let db = Database::open(uri(&path)).unwrap();
        let mut conn = db.connect().unwrap();
        conn.query("CREATE (:Seed)").unwrap();
        db.checkpoint().unwrap();
        locked(Database::open(uri(&path)));
        drop(db);
        locked(Database::open(uri(&path)));
        drop(conn);
        let reopened = Database::open(uri(&path)).unwrap();
        assert_eq!(
            reopened
                .connect()
                .unwrap()
                .query("MATCH (n:Seed) RETURN count(n)")
                .unwrap()
                .rows
                .len(),
            1
        );
        drop(reopened);
        if bundle {
            assert_eq!(
                fs::read_dir(dir.path()).unwrap().count(),
                1,
                ".cl has no sidecar"
            );
        }
    }
    let a = Database::open(":memory:").unwrap();
    let b = Database::open(":memory:").unwrap();
    drop((a, b));
}

#[test]
fn independent_process_contends_and_crash_releases_lock() {
    for bundle in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(if bundle { "child.cl" } else { "child" });
        let db = Database::open(uri(&path)).unwrap();
        let out = child(&path, "locked");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        drop(db);
        let out = child(&path, "open");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );

        let mut process = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "storage_lock_child", "--nocapture"])
            .env("CYPHERLITE_LOCK_CHILD", "hold")
            .env("CYPHERLITE_LOCK_PATH", &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = BufReader::new(process.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert!(
                reader.read_line(&mut line).unwrap() > 0,
                "child exited before locking"
            );
            if line.contains("READY") {
                break;
            }
        }
        locked(Database::open(uri(&path)));
        process.kill().unwrap();
        process.wait().unwrap();
        drop(Database::open(uri(&path)).unwrap());
    }
}

#[test]
fn wal_override_is_a_second_ownership_identity() {
    let dir = tempfile::tempdir().unwrap();
    let wal = dir.path().join("wal-shared");
    let root_a = dir.path().join("a");
    let root_b = dir.path().join("b");
    let a = Database::open_with(
        uri(&root_a),
        OpenOptions {
            wal_dir: Some(wal.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    locked(Database::open_with(
        uri(&root_b),
        OpenOptions {
            wal_dir: Some(wal.clone()),
            ..Default::default()
        },
    ));
    locked(Database::open(uri(&root_a)));
    drop(a);
    let b = Database::open_with(
        uri(&root_b),
        OpenOptions {
            wal_dir: Some(wal.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    drop(b);
    // The WAL override may alias the root itself; it must reuse the lease.
    drop(
        Database::open_with(
            uri(&root_a),
            OpenOptions {
                wal_dir: Some(root_a.clone()),
                ..Default::default()
            },
        )
        .unwrap(),
    );
}

#[test]
fn closed_native_root_cannot_be_attached_or_reset_as_external_wal() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    let db = Database::open(uri(&second)).unwrap();
    db.connect().unwrap().query("CREATE (:Preserved)").unwrap();
    db.checkpoint().unwrap();
    drop(db);
    drop(Database::open(uri(&first)).unwrap());
    let before = fs::read(second.join("manifest.json")).unwrap();
    let options = || OpenOptions {
        wal_dir: Some(second.clone()),
        ..Default::default()
    };
    assert!(matches!(
        Database::open_with(uri(&first), options()),
        Err(Error::Storage(_))
    ));
    assert!(matches!(
        cypherlite_storage::reset_uri(uri(&first), Some(&second)),
        Err(Error::Storage(_))
    ));
    assert_eq!(fs::read(second.join("manifest.json")).unwrap(), before);
    assert!(!first.join(".reset-incomplete").exists());
    let reopened = Database::open(uri(&second)).unwrap();
    assert_eq!(
        reopened
            .connect()
            .unwrap()
            .query("MATCH (n:Preserved) RETURN count(n)")
            .unwrap()
            .rows[0][0]
            .as_int(),
        Some(1)
    );
}

#[test]
fn closed_root_as_external_wal_is_rejected_before_creating_root_marker() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("legacy-root");
    let other = dir.path().join("independent");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("manifest.json"), b"legacy root evidence").unwrap();
    drop(Database::open(uri(&other)).unwrap());
    let old_marker = fs::read(other.join(LOCK_MARKER)).unwrap();

    let error = Database::open_with(
        uri(&root),
        OpenOptions {
            wal_dir: Some(other.clone()),
            ..Default::default()
        },
    )
    .err()
    .unwrap();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert!(!root.join(LOCK_MARKER).exists());
    let error = cypherlite_storage::reset_uri(uri(&root), Some(&other)).unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert!(!root.join(LOCK_MARKER).exists());
    assert_eq!(fs::read(other.join(LOCK_MARKER)).unwrap(), old_marker);
    assert_eq!(
        fs::read(root.join("manifest.json")).unwrap(),
        b"legacy root evidence"
    );
    assert!(!root.join(".reset-incomplete").exists());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn closed_nested_root_at_default_wal_is_not_owned_by_external_wal() {
    for marked_root in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("outer");
        let external = dir.path().join("external-wal");
        let options = || OpenOptions {
            wal_dir: Some(external.clone()),
            ..Default::default()
        };
        let outer = Database::open_with(uri(&root), options()).unwrap();
        outer.checkpoint().unwrap();
        drop(outer);
        if !marked_root {
            fs::remove_file(root.join(LOCK_MARKER)).unwrap();
        }
        let detached = dir.path().join("detached");
        let independent = Database::open(uri(&detached)).unwrap();
        independent
            .connect()
            .unwrap()
            .query("CREATE (:Preserved)")
            .unwrap();
        independent.checkpoint().unwrap();
        drop(independent);
        fs::rename(&detached, root.join("wal")).unwrap();
        let before = fs::read(root.join("wal").join(LOCK_MARKER)).unwrap();
        let manifest = fs::read(root.join("wal/manifest.json")).unwrap();
        let outer_marker = marked_root.then(|| fs::read(root.join(LOCK_MARKER)).unwrap());
        let external_marker = fs::read(external.join(LOCK_MARKER)).unwrap();

        let error = Database::open_with(uri(&root), options()).err().unwrap();
        assert!(matches!(error, Error::Storage(_)), "{error}");
        let error = cypherlite_storage::reset_uri(uri(&root), Some(&external)).unwrap_err();
        assert!(matches!(error, Error::Storage(_)), "{error}");
        assert_eq!(outer_marker, fs::read(root.join(LOCK_MARKER)).ok());
        assert_eq!(
            fs::read(external.join(LOCK_MARKER)).unwrap(),
            external_marker
        );
        assert_eq!(
            fs::read(root.join("wal").join(LOCK_MARKER)).unwrap(),
            before
        );
        assert_eq!(fs::read(root.join("wal/manifest.json")).unwrap(), manifest);
        assert!(!root.join(".reset-incomplete").exists());
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            2,
            "no tombstones"
        );
    }
}

#[test]
fn legacy_root_with_typed_disjoint_wal_still_opens_and_resets() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("legacy-root");
    let wal = dir.path().join("external-wal");
    let options = || OpenOptions {
        wal_dir: Some(wal.clone()),
        ..Default::default()
    };
    let db = Database::open_with(uri(&root), options()).unwrap();
    db.checkpoint().unwrap();
    drop(db);
    fs::write(root.join(LOCK_MARKER), b"").unwrap();
    drop(Database::open_with(uri(&root), options()).unwrap());
    assert_eq!(fs::read(root.join(LOCK_MARKER)).unwrap(), b"");
    #[cfg(unix)]
    {
        cypherlite_storage::reset_uri(uri(&root), Some(&wal)).unwrap();
        assert!(!root.exists());
        assert!(!wal.exists());
    }
    #[cfg(windows)]
    assert!(matches!(
        cypherlite_storage::reset_uri(uri(&root), Some(&wal)),
        Err(Error::Storage(_))
    ));
}

#[test]
fn ambiguous_old_empty_lock_cannot_be_claimed_as_disjoint_wal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let legacy = dir.path().join("legacy");
    drop(Database::open(uri(&root)).unwrap());
    fs::create_dir(&legacy).unwrap();
    fs::write(legacy.join(LOCK_MARKER), b"").unwrap();
    let options = OpenOptions {
        wal_dir: Some(legacy.clone()),
        ..Default::default()
    };
    assert!(matches!(
        Database::open_with(uri(&root), options),
        Err(Error::Storage(_))
    ));
    assert!(matches!(
        cypherlite_storage::reset_uri(uri(&root), Some(&legacy)),
        Err(Error::Storage(_))
    ));
    assert_eq!(fs::read(legacy.join(LOCK_MARKER)).unwrap(), b"");
    assert!(root.exists());
}

#[cfg(windows)]
#[test]
fn external_wal_reset_fails_closed_without_namespace_barrier() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let wal = dir.path().join("external");
    drop(
        Database::open_with(
            uri(&root),
            OpenOptions {
                wal_dir: Some(wal.clone()),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let before = (
        fs::read(root.join(LOCK_MARKER)).unwrap(),
        fs::read(wal.join(LOCK_MARKER)).unwrap(),
    );
    let error = cypherlite_storage::reset_uri(uri(&root), Some(&wal)).unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert_eq!(
        before,
        (
            fs::read(root.join(LOCK_MARKER)).unwrap(),
            fs::read(wal.join(LOCK_MARKER)).unwrap()
        )
    );
    assert!(!root.join(".reset-incomplete").exists());
    drop(Database::open(uri(&root)).unwrap());
    cypherlite_storage::reset_uri(uri(&root), None).unwrap();
}

#[test]
fn reset_with_wal_override_contends_before_deleting_either_identity() {
    let dir = tempfile::tempdir().unwrap();
    let root_a = dir.path().join("a");
    let root_b = dir.path().join("b");
    let wal = dir.path().join("shared-wal");
    let owner = Database::open_with(
        uri(&root_a),
        OpenOptions {
            wal_dir: Some(wal.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    drop(Database::open(uri(&root_b)).unwrap());
    #[cfg(unix)]
    assert!(matches!(
        cypherlite_storage::reset_uri(uri(&root_b), Some(&wal)),
        Err(Error::Locked(_))
    ));
    #[cfg(windows)]
    assert!(matches!(
        cypherlite_storage::reset_uri(uri(&root_b), Some(&wal)),
        Err(Error::Storage(_))
    ));
    assert!(root_b.exists());
    assert!(wal.exists());
    drop(owner);
    #[cfg(windows)]
    {
        assert!(matches!(
            cypherlite_storage::reset_uri(uri(&root_b), Some(&wal)),
            Err(Error::Storage(_))
        ));
        assert!(root_b.exists() && wal.exists());
    }
    #[cfg(unix)]
    {
        cypherlite_storage::reset_uri(uri(&root_b), Some(&wal)).unwrap();
        assert!(!root_b.exists());
        assert!(!wal.exists());
    }
}

#[test]
fn external_wal_with_legacy_nested_owner_refuses_open_and_reset() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let wal = dir.path().join("external-wal");
    // The parent predates this WAL override. Both children acquire their
    // ownership before the external WAL directory has a marker identity.
    drop(Database::open(uri(&root)).unwrap());
    let child = wal.join("legacy").join("child");
    let bundle = wal.join("legacy").join("child.CL");
    let native_owner = Database::open(uri(&child)).unwrap();
    let bundle_owner = Database::open(uri(&bundle)).unwrap();
    for owner in [&native_owner, &bundle_owner] {
        owner
            .connect()
            .unwrap()
            .query("CREATE (:Independent)")
            .unwrap();
    }
    assert!(!wal.join(LOCK_MARKER).exists());

    let options = || OpenOptions {
        wal_dir: Some(wal.clone()),
        ..Default::default()
    };
    let assert_refused = || {
        let open_error = Database::open_with(uri(&root), options()).err().unwrap();
        assert!(matches!(open_error, Error::Storage(_)), "{open_error}");
        let reset_error = cypherlite_storage::reset_uri(uri(&root), Some(&wal)).unwrap_err();
        assert!(matches!(reset_error, Error::Storage(_)), "{reset_error}");
        assert!(root.exists());
        assert!(wal.exists());
        assert!(!root.join(".reset-incomplete").exists());
        assert!(!wal.join(LOCK_MARKER).exists());
        assert!(!fs::read_dir(dir.path()).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".reset-")));
    };
    assert_refused();
    for owner in [&native_owner, &bundle_owner] {
        assert_eq!(
            owner
                .connect()
                .unwrap()
                .query("MATCH (n:Independent) RETURN count(n)")
                .unwrap()
                .rows[0][0]
                .as_int(),
            Some(1)
        );
    }
    drop(native_owner);
    cypherlite_storage::reset_uri(uri(&child), None).unwrap();
    // The bundle is protected independently of the Native child marker.
    assert_refused();
    drop(bundle_owner);
    assert_eq!(
        Database::open(uri(&bundle))
            .unwrap()
            .connect()
            .unwrap()
            .query("MATCH (n:Independent) RETURN count(n)")
            .unwrap()
            .rows[0][0]
            .as_int(),
        Some(1)
    );
    cypherlite_storage::reset_uri(uri(&bundle), None).unwrap();
    // The old markerless WAL remains ambiguous even after removing its
    // nested owners. Explicitly retire that empty directory before reusing
    // the path as a newly created WAL identity.
    fs::remove_dir_all(&wal).unwrap();
    drop(Database::open_with(uri(&root), options()).unwrap());
    #[cfg(unix)]
    cypherlite_storage::reset_uri(uri(&root), Some(&wal)).unwrap();
}

fn assert_markerless_wal_preserves_live_child(root: &Path, wal: &Path) {
    drop(Database::open(uri(root)).unwrap());
    let child = wal.join("legacy").join("child");
    let owner = Database::open(uri(&child)).unwrap();
    owner
        .connect()
        .unwrap()
        .query("CREATE (:Independent)")
        .unwrap();
    assert!(!wal.join(LOCK_MARKER).exists());

    let error = Database::open_with(
        uri(root),
        OpenOptions {
            wal_dir: Some(wal.to_path_buf()),
            ..Default::default()
        },
    )
    .err()
    .unwrap();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert!(
        !wal.join(LOCK_MARKER).exists(),
        "failed open marked WAL ancestor"
    );
    assert_eq!(
        owner
            .connect()
            .unwrap()
            .query("MATCH (n:Independent) RETURN count(n)")
            .unwrap()
            .rows[0][0]
            .as_int(),
        Some(1)
    );
    drop(owner);
    let reopened = Database::open(uri(&child)).unwrap();
    assert_eq!(
        reopened
            .connect()
            .unwrap()
            .query("MATCH (n:Independent) RETURN count(n)")
            .unwrap()
            .rows[0][0]
            .as_int(),
        Some(1)
    );
}

#[test]
fn disjoint_markerless_wal_with_live_child_stays_markerless_after_failed_open() {
    let dir = tempfile::tempdir().unwrap();
    assert_markerless_wal_preserves_live_child(
        &dir.path().join("root"),
        &dir.path().join("external-wal"),
    );
}

#[test]
fn case_variant_markerless_wal_with_live_child_stays_markerless_after_failed_open() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Graph");
    let wal = dir.path().join("graph");
    fs::create_dir(&root).unwrap();
    fs::create_dir_all(&wal).unwrap();
    // Case-insensitive volumes resolve these spellings to the same directory.
    if fs::canonicalize(&root).unwrap() != fs::canonicalize(&wal).unwrap() {
        assert_markerless_wal_preserves_live_child(&root, &wal);
    }
}

#[test]
fn external_wal_lock_names_are_checked_by_physical_identity() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let wal = dir.path().join("external-wal");
    let options = || OpenOptions {
        wal_dir: Some(wal.clone()),
        ..Default::default()
    };
    drop(Database::open_with(uri(&root), options()).unwrap());
    let nested = wal.join("legacy");
    fs::create_dir(&nested).unwrap();
    fs::write(
        nested.join(LOCK_MARKER.to_ascii_uppercase()),
        b"legacy identity",
    )
    .unwrap();
    let error = Database::open_with(uri(&root), options()).err().unwrap();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    let error = cypherlite_storage::reset_uri(uri(&root), Some(&wal)).unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert_eq!(
        fs::read(nested.join(LOCK_MARKER.to_ascii_uppercase())).unwrap(),
        b"legacy identity"
    );
    fs::remove_dir_all(&nested).unwrap();

    // On a case-sensitive volume, the uppercase marker is a separate marker
    // beside the WAL's own marker. On a case-insensitive volume it aliases
    // that identity.
    let uppercase = wal.join(LOCK_MARKER.to_ascii_uppercase());
    if !uppercase.exists() {
        fs::write(&uppercase, b"other identity").unwrap();
        let error = Database::open_with(uri(&root), options()).err().unwrap();
        assert!(matches!(error, Error::Storage(_)), "{error}");
        let error = cypherlite_storage::reset_uri(uri(&root), Some(&wal)).unwrap_err();
        assert!(matches!(error, Error::Storage(_)), "{error}");
        assert_eq!(fs::read(&uppercase).unwrap(), b"other identity");
        fs::remove_file(&uppercase).unwrap();
    }
    drop(Database::open_with(uri(&root), options()).unwrap());
    #[cfg(unix)]
    cypherlite_storage::reset_uri(uri(&root), Some(&wal)).unwrap();
}

#[cfg(unix)]
#[test]
fn default_wal_symlink_to_external_nested_owner_refuses_reset() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let wal = dir.path().join("external-wal");
    drop(Database::open(uri(&root)).unwrap());
    fs::remove_dir_all(root.join("wal")).unwrap();
    let child = wal.join("legacy").join("child");
    let owner = Database::open(uri(&child)).unwrap();
    symlink(&wal, root.join("wal")).unwrap();
    let error = cypherlite_storage::reset_uri(uri(&root), None).unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    let error = Database::open(uri(&root)).err().unwrap();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert!(fs::symlink_metadata(root.join("wal"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(child.join(LOCK_MARKER).exists());
    assert!(!root.join(".reset-incomplete").exists());
    assert!(!fs::read_dir(dir.path()).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".reset-")));
    drop(owner);
}

#[test]
fn live_reset_must_not_unlink_a_locked_root_or_bundle() {
    for bundle in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(if bundle { "reset.cl" } else { "reset" });
        let db = Database::open(uri(&path)).unwrap();
        let mut c = db.connect().unwrap();
        c.query("CREATE (:Preserved)").unwrap();
        db.checkpoint().unwrap();
        // The baseline is metadata only. While the owner holds its exclusive
        // byte-range lock, opening a fresh handle to the `.cl` bytes returns
        // ERROR_LOCK_VIOLATION (os error 33) on Windows, so the test never
        // reads the locked bundle through a new handle. `fs::metadata` and
        // `Path::exists` use directory metadata and are unaffected by the lock.
        let baseline_len = fs::metadata(&path).unwrap().len();
        assert!(matches!(
            cypherlite_storage::reset_uri(uri(&path), None),
            Err(Error::Locked(_))
        ));
        // Non-mutation while locked: the file is neither truncated nor removed.
        if bundle {
            assert_eq!(fs::metadata(&path).unwrap().len(), baseline_len);
        }
        assert!(path.exists());
        drop((c, db));
        // The refused reset left the database readable with its data intact.
        // Raw byte equality across a lock cycle is not a valid non-mutation
        // signal: every open republishes a bundle superblock generation and the
        // catalog embeds a randomly named temp entry, so two opens of identical
        // bytes legitimately differ. Reopening and querying the data proves the
        // bundle was neither mutated nor removed.
        let reopened = Database::open(uri(&path)).unwrap();
        assert_eq!(
            reopened
                .connect()
                .unwrap()
                .query("MATCH (n:Preserved) RETURN count(n)")
                .unwrap()
                .rows
                .len(),
            1
        );
        drop(reopened);
        cypherlite_storage::reset_uri(uri(&path), None).unwrap();
        assert!(!path.exists());
    }
}

#[cfg(unix)]
#[test]
fn filesystem_aliases_contend_and_final_lock_symlink_is_refused() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let db = Database::open(uri(&root)).unwrap();
    let alias = dir.path().join("alias");
    symlink(&root, &alias).unwrap();
    locked(Database::open(uri(&alias)));
    locked(Database::open(&format!("file://{}", root.display())));
    drop(db);

    let bundle = dir.path().join("original.cl");
    let db = Database::open(uri(&bundle)).unwrap();
    let link = dir.path().join("linked.cl");
    symlink(&bundle, &link).unwrap();
    let hardlink = dir.path().join("hardlink.cl");
    fs::hard_link(&bundle, &hardlink).unwrap();
    locked(Database::open(uri(&link)));
    locked(Database::open(uri(&hardlink)));
    locked(Database::open(&format!("file://{}", bundle.display())));
    drop(db);

    let bad = dir.path().join("bad");
    fs::create_dir(&bad).unwrap();
    symlink(root.join(LOCK_MARKER), bad.join(LOCK_MARKER)).unwrap();
    assert!(!matches!(Database::open(uri(&bad)), Err(Error::Locked(_))));
    assert!(Database::open(uri(&bad)).is_err());
    let dangling = dir.path().join("dangling.cl");
    symlink(dir.path().join("missing.cl"), &dangling).unwrap();
    assert!(Database::open(uri(&dangling)).is_err());
    assert!(!dir.path().join("missing.cl").exists());

    let shared_wal = dir.path().join("shared-wal");
    let owner = Database::open_with(
        uri(&root),
        OpenOptions {
            wal_dir: Some(shared_wal.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let wal_alias = dir.path().join("wal-alias");
    symlink(&shared_wal, &wal_alias).unwrap();
    locked(Database::open_with(
        uri(&dir.path().join("other")),
        OpenOptions {
            wal_dir: Some(wal_alias),
            ..Default::default()
        },
    ));
    drop(owner);
}

#[test]
fn relative_and_absolute_native_paths_share_one_owner() {
    let cwd = std::env::current_dir().unwrap();
    let dir = tempfile::tempdir_in(&cwd).unwrap();
    let absolute = dir.path().join("db");
    let relative = absolute.strip_prefix(&cwd).unwrap();
    let db = Database::open(uri(&absolute)).unwrap();
    locked(Database::open(uri(relative)));
    drop(db);
    drop(Database::open(uri(relative)).unwrap());
}

#[test]
fn concurrent_independent_native_owners_reopen_immediately() {
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                let dir = tempfile::tempdir().unwrap();
                for n in 0..32 {
                    let root = dir.path().join(format!("db-{n}"));
                    if n % 2 == 0 {
                        let owner = Database::open(uri(&root)).unwrap();
                        locked(Database::open(uri(&root)));
                        drop(owner);
                        drop(Database::open(uri(&root)).unwrap());
                    } else {
                        let wal = dir.path().join(format!("wal-{n}"));
                        let options = || OpenOptions {
                            wal_dir: Some(wal.clone()),
                            ..Default::default()
                        };
                        let owner = Database::open_with(uri(&root), options()).unwrap();
                        let next = dir.path().join(format!("next-{n}"));
                        locked(Database::open_with(uri(&next), options()));
                        drop(owner);
                        drop(Database::open_with(uri(&next), options()).unwrap());
                    }
                }
            });
        }
    });
}

#[test]
fn failed_open_releases_lease_without_mutating_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("broken.cl");
    fs::write(&bundle, b"not a bundle").unwrap();
    for _ in 0..2 {
        assert!(matches!(
            Database::open(uri(&bundle)),
            Err(Error::Storage(_))
        ));
        assert_eq!(fs::read(&bundle).unwrap(), b"not a bundle");
    }
    fs::remove_file(&bundle).unwrap();
    drop(Database::open(uri(&bundle)).unwrap());

    let native = dir.path().join("native");
    fs::create_dir(&native).unwrap();
    fs::write(native.join("graph.json"), b"{}").unwrap();
    fs::write(native.join("nodes.parquet"), b"bad").unwrap();
    for _ in 0..2 {
        assert!(matches!(
            Database::open(uri(&native)),
            Err(Error::Storage(_))
        ));
    }
    fs::remove_file(native.join("nodes.parquet")).unwrap();
    fs::remove_file(native.join("graph.json")).unwrap();
    // Clearing the legacy data also removes its now-ambiguous empty marker.
    fs::remove_dir_all(&native).unwrap();
    drop(Database::open(uri(&native)).unwrap());
}

#[test]
fn unrelated_empty_ancestor_lock_is_not_a_database_identity() {
    // pytest creates `pytest-0/.lock` inside its temporary parent. That
    // generic name must not be mistaken for a CypherLite storage identity.
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("pytest-0");
    fs::create_dir(&parent).unwrap();
    fs::write(parent.join(".lock"), b"").unwrap();
    let root = parent.join("db");

    let db = Database::open(uri(&root)).unwrap();
    db.connect()
        .unwrap()
        .query("CREATE (:Kept {n: 1})")
        .unwrap();
    db.checkpoint().unwrap();
    // The open really acquired ownership of the subdirectory store.
    locked(Database::open(uri(&root)));
    drop(db);

    let reopened = Database::open(uri(&root)).unwrap();
    assert_eq!(
        reopened
            .connect()
            .unwrap()
            .query("MATCH (n:Kept) RETURN count(n)")
            .unwrap()
            .rows[0][0]
            .as_int(),
        Some(1)
    );
    drop(reopened);

    // CypherLite wrote its own unique marker; the unrelated `.lock` stays.
    assert!(root.join(LOCK_MARKER).exists());
    assert!(parent.join(".lock").exists());
    cypherlite_storage::reset_uri(uri(&root), None).unwrap();
    assert!(!root.exists());
    assert!(
        parent.join(".lock").exists(),
        "the unrelated .lock must not be touched"
    );
}

#[test]
fn ancestor_cypherlite_marker_still_fails_closed_as_nested() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("owner");
    fs::create_dir(&parent).unwrap();
    // A real CypherLite identity in an ancestor still marks a nested open,
    // even when the outer owner is closed.
    fs::write(parent.join(LOCK_MARKER), b"").unwrap();
    let root = parent.join("db");
    let error = Database::open(uri(&root)).err().unwrap();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert!(!root.exists(), "refused before creating the nested root");
}

#[cfg(unix)]
fn assert_private_tree(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    fn walk(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        for item in fs::read_dir(path).unwrap() {
            let item = item.unwrap();
            let meta = item.metadata().unwrap();
            if meta.is_dir() {
                assert_eq!(
                    meta.permissions().mode() & 0o777,
                    0o700,
                    "{}",
                    item.path().display()
                );
                walk(&item.path());
            } else {
                assert_eq!(
                    meta.permissions().mode() & 0o777,
                    0o600,
                    "{}",
                    item.path().display()
                );
            }
        }
    }
    walk(root);
}

#[cfg(unix)]
#[test]
fn new_files_are_private_even_with_permissive_umask_existing_modes_stay() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("parents").join("nested").join("db");
    let out = child(&path, "modes");
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_private_tree(&path);
    assert_eq!(
        fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    let existing = dir.path().join("existing");
    fs::create_dir(&existing).unwrap();
    fs::set_permissions(&existing, fs::Permissions::from_mode(0o755)).unwrap();
    let file = dir.path().join("already.cl");
    drop(Database::open(uri(&file)).unwrap());
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    drop(Database::open(uri(&file)).unwrap());
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o644
    );
    drop(Database::open(uri(&existing)).unwrap());
    assert_eq!(
        fs::metadata(&existing).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let existing_lock = existing.join(LOCK_MARKER);
    fs::set_permissions(&existing_lock, fs::Permissions::from_mode(0o644)).unwrap();
    drop(Database::open(uri(&existing)).unwrap());
    assert_eq!(
        fs::metadata(&existing_lock).unwrap().permissions().mode() & 0o777,
        0o644
    );
    let bundle = dir.path().join("private.cl");
    let out = child(&bundle, "bundle_modes");
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::metadata(&bundle).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
