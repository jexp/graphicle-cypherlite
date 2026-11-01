//! The reset process-handoff arc of the native-ownership tests: the
//! ready/resume barrier harness, the fresh-root handoff, and the
//! cross-process crash legs — extracted from `tests.rs` (the
//! no-1k-line rule, LABS-311). Shares its fixtures and helpers
//! through `use super::*`.

use super::*;

/// The ready/resume barrier harness the handoff tests share: spawn the
/// reset on its own thread, pausing at every staged move until the test
/// resumes it — one spelling of the channel pair and the step callback.
fn spawn_reset_with_barrier(
    root: PathBuf,
    wal: Option<PathBuf>,
) -> (
    std::thread::JoinHandle<Result<()>>,
    std::sync::mpsc::Receiver<MoveKind>,
    std::sync::mpsc::Sender<()>,
) {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let reset = std::thread::spawn(move || {
        reset_native_with(&root, wal.as_deref(), |step| {
            ready_tx.send(step).unwrap();
            resume_rx.recv().unwrap();
            Ok(())
        })
    });
    (reset, ready_rx, resume_tx)
}

#[test]
fn reset_handoff_keeps_fresh_root_and_wal_while_removing_only_old_tree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let uri = root.to_str().unwrap();
    let old = parse_uri(uri).unwrap();
    let old_vfs = old.vfs();
    let mut old_file = old_vfs.open("wal/old.log", VfsOpen::create_new()).unwrap();
    old_file.write_all(b"old").unwrap();
    drop((old_file, old_vfs, old));

    let (reset, ready_rx, resume_tx) = spawn_reset_with_barrier(root.clone(), None);
    assert_eq!(ready_rx.recv().unwrap(), MoveKind::Root);
    assert!(!root.exists());
    let fresh = parse_uri(uri).unwrap();
    let vfs = fresh.vfs();
    let mut file = vfs.open("wal/fresh.log", VfsOpen::create_new()).unwrap();
    file.write_all(b"new").unwrap();
    drop((file, vfs));
    resume_tx.send(()).unwrap();
    reset.join().unwrap().unwrap();
    assert_eq!(std::fs::read(root.join("wal/fresh.log")).unwrap(), b"new");
    assert!(matches!(parse_uri(uri), Err(Error::Locked(_))));
    drop(fresh);
    drop(parse_uri(uri).unwrap());
}

#[test]
#[cfg(unix)]
fn reset_external_wal_handoff_never_exposes_old_wal_to_fresh_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let wal = dir.path().join("external-wal");
    let other = dir.path().join("other");
    drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap());
    let (reset, ready_rx, resume_tx) = spawn_reset_with_barrier(root.clone(), Some(wal.clone()));
    assert_eq!(ready_rx.recv().unwrap(), MoveKind::Wal);
    assert!(matches!(
        parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())),
        Err(Error::Locked(_))
    ));
    assert!(!wal.exists(), "old WAL was staged before the root");
    // A different root may create a fresh WAL, but it cannot access the
    // staged old WAL and must release the new one before our fresh open.
    let independent = parse_uri_with_wal(other.to_str().unwrap(), Some(wal.clone())).unwrap();
    drop(independent);
    resume_tx.send(()).unwrap();
    assert_eq!(ready_rx.recv().unwrap(), MoveKind::Root);
    let fresh = parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap();
    let (vfs, _) = fresh.wal_vfs();
    let mut file = vfs.open("fresh.log", VfsOpen::create_new()).unwrap();
    file.write_all(b"new").unwrap();
    drop((file, vfs));
    resume_tx.send(()).unwrap();
    reset.join().unwrap().unwrap();
    assert_eq!(std::fs::read(wal.join("fresh.log")).unwrap(), b"new");
    drop(fresh);
}

/// The child-process crash leg: run the reset and abort exactly when
/// the requested move phase begins — no Drop of either lock or the
/// staged tombstones (the abrupt-termination simulation the journal
/// recovery tests recover from).
#[cfg(unix)]
fn crash_reset_at(root: &std::path::Path, wal: &std::path::Path, phase: MoveKind) -> ! {
    let _ = reset_native_with(root, Some(wal), |step| {
        if step == phase {
            std::process::exit(0);
        }
        Ok(())
    });
    panic!("reset did not reach requested crash phase {phase:?}");
}

/// The harness filter that selects [`reset_child`] in a spawned copy of
/// this test binary: the test's path without the crate prefix
/// `module_path!()` carries, so the filter tracks this module's location
/// across file moves instead of a second hand-maintained spelling.
#[cfg(unix)]
fn child_test_name() -> String {
    format!(
        "{}::reset_child",
        module_path!()
            .split_once("::")
            .map_or(module_path!(), |(_, rest)| rest)
    )
}

#[test]
fn reset_child() {
    let Ok(mode) = std::env::var("CYPHERLITE_RESET_CHILD") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("CYPHERLITE_RESET_ROOT").unwrap());
    let wal = PathBuf::from(std::env::var_os("CYPHERLITE_RESET_WAL").unwrap());
    match mode.as_str() {
        #[cfg(unix)]
        "crash-wal" => crash_reset_at(&root, &wal, MoveKind::Wal),
        #[cfg(unix)]
        "crash-root" => crash_reset_at(&root, &wal, MoveKind::Root),
        "locked" => assert!(matches!(
            parse_uri_with_wal(root.to_str().unwrap(), Some(wal)),
            Err(Error::Locked(_))
        )),
        "fresh" => {
            let target = parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap();
            assert!(!wal.join("old").exists());
            target
                .wal_vfs()
                .0
                .open("fresh", VfsOpen::create_new())
                .unwrap()
                .write_all(b"new")
                .unwrap();
        }
        _ => panic!("unknown reset child mode"),
    }
}

#[test]
#[cfg(unix)]
fn process_crash_at_each_reset_stage_never_attaches_old_wal() {
    for phase in ["wal", "root"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("db");
        let wal = dir.path().join("external");
        let target = parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap();
        target
            .wal_vfs()
            .0
            .open("old", VfsOpen::create_new())
            .unwrap()
            .write_all(b"old")
            .unwrap();
        drop(target);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &child_test_name(), "--nocapture"])
            .env("CYPHERLITE_RESET_CHILD", format!("crash-{phase}"))
            .env("CYPHERLITE_RESET_ROOT", &root)
            .env("CYPHERLITE_RESET_WAL", &wal)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(!wal.join("old").exists(), "{phase}");
        let stranded = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path().join("data/old"))
            .find(|path| path.exists())
            .expect("the old WAL is retained in a tombstone");
        assert_eq!(std::fs::read(stranded).unwrap(), b"old");
        if phase == "wal" {
            assert!(root.join(RESET_INCOMPLETE).exists());
            assert!(matches!(
                parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())),
                Err(Error::Storage(_))
            ));
        } else {
            let fresh = parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap();
            assert!(!wal.join("old").exists());
            drop(fresh);
        }
    }
}

#[test]
#[cfg(unix)]
fn reset_wal_first_cross_process_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let wal = dir.path().join("external-wal");
    let target = parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap();
    target
        .wal_vfs()
        .0
        .open("old", VfsOpen::create_new())
        .unwrap()
        .write_all(b"old")
        .unwrap();
    drop(target);
    let (reset, ready_rx, resume_tx) = spawn_reset_with_barrier(root.clone(), Some(wal.clone()));
    let run_child = |mode| {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &child_test_name(), "--nocapture"])
            .env("CYPHERLITE_RESET_CHILD", mode)
            .env("CYPHERLITE_RESET_ROOT", &root)
            .env("CYPHERLITE_RESET_WAL", &wal)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    assert_eq!(ready_rx.recv().unwrap(), MoveKind::Wal);
    run_child("locked");
    resume_tx.send(()).unwrap();
    assert_eq!(ready_rx.recv().unwrap(), MoveKind::Root);
    run_child("fresh");
    resume_tx.send(()).unwrap();
    reset.join().unwrap().unwrap();
    assert_eq!(std::fs::read(wal.join("fresh")).unwrap(), b"new");
    assert!(!wal.join("old").exists());
}

#[test]
#[cfg(unix)]
fn failed_reset_after_fresh_root_keeps_old_wal_isolated_and_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let wal = dir.path().join("external-wal");
    let target = parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap();
    let (graph, memory_wal) = person_ada();
    checkpoint_with_lsn(&target, &graph, memory_wal.lsn, None).unwrap();
    target
        .wal_vfs()
        .0
        .open("old", VfsOpen::create_new())
        .unwrap()
        .write_all(b"old")
        .unwrap();
    drop(target);
    let error = reset_native_with(&root, Some(&wal), |step| {
        if step == MoveKind::Root {
            let fresh = parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone()))?;
            fresh
                .wal_vfs()
                .0
                .open("fresh", VfsOpen::create_new())?
                .write_all(b"new")?;
            return Err(Error::io("injected after fresh root"));
        }
        Ok(())
    })
    .unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert_eq!(std::fs::read(wal.join("fresh")).unwrap(), b"new");
    assert!(!wal.join("old").exists());
    let mut old_root = None;
    let mut old_wal = None;
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(".db.reset-") {
            old_root = Some(entry.path().join("data"));
        } else if name.starts_with(".external-wal.reset-") {
            old_wal = Some(entry.path().join("data"));
        }
    }
    let old_root = old_root.unwrap();
    let old_wal = old_wal.unwrap();
    assert!(error.to_string().contains(&old_root.display().to_string()));
    assert!(error.to_string().contains(&old_wal.display().to_string()));
    assert_eq!(std::fs::read(old_wal.join("old")).unwrap(), b"old");
    assert!(old_root.join(RESET_INCOMPLETE).exists());
    reset_uri(root.to_str().unwrap(), Some(&wal)).unwrap();
    std::fs::rename(&old_root, &root).unwrap();
    std::fs::rename(&old_wal, &wal).unwrap();
    std::fs::remove_file(root.join(RESET_INCOMPLETE)).unwrap();
    let recovered = parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap();
    assert_eq!(load(&recovered).unwrap().live_node_count(), 1);
    assert_eq!(std::fs::read(wal.join("old")).unwrap(), b"old");
}
