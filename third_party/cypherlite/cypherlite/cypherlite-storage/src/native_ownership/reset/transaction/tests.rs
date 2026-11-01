use super::super::ResetMoves;
use super::*;
use crate::native_ownership::tests::assert_no_reset_residue;
use crate::parse_uri_with_wal;

use std::os::unix::fs::symlink;

/// The swap-attack fixture: an owned, locked root plus a decoy tree,
/// either of which a missed recheck could silently substitute for the
/// other. `swap_decoy_in` renames the owned tree away and links the
/// decoy in under the root's name.
struct SwapAttack {
    root: PathBuf,
    moved: PathBuf,
    unrelated: PathBuf,
}

fn swap_attack_fixture(dir: &Path) -> SwapAttack {
    let fixture = SwapAttack {
        root: dir.join("root"),
        moved: dir.join("moved"),
        unrelated: dir.join("unrelated"),
    };
    drop(parse_uri_with_wal(fixture.root.to_str().unwrap(), None).unwrap());
    std::fs::write(fixture.root.join("sentinel"), b"owned").unwrap();
    std::fs::create_dir(&fixture.unrelated).unwrap();
    std::fs::write(fixture.unrelated.join("sentinel"), b"unrelated").unwrap();
    fixture
}

/// Swap the decoy in under the owned root's name.
fn swap_decoy_in(fixture: &SwapAttack) {
    std::fs::rename(&fixture.root, &fixture.moved).unwrap();
    symlink(&fixture.unrelated, &fixture.root).unwrap();
}

/// The recheck under test refused the swap: the decoy survives
/// untouched and the owned tree stays at its moved name.
fn assert_swap_refused(fixture: &SwapAttack) {
    assert_eq!(
        std::fs::read_link(&fixture.root).unwrap(),
        fixture.unrelated
    );
    assert_eq!(
        std::fs::read(fixture.unrelated.join("sentinel")).unwrap(),
        b"unrelated"
    );
    assert_eq!(
        std::fs::read(fixture.moved.join("sentinel")).unwrap(),
        b"owned"
    );
}

#[test]
fn reset_rechecks_requested_root_after_lease_and_before_staging() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = swap_attack_fixture(dir.path());
    let plan = ResetPlan::preflight(&fixture.root, None).unwrap();
    swap_decoy_in(&fixture);
    let err = ResetTransaction::new(plan, &mut |_, _| Ok(()))
        .err()
        .unwrap();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert_swap_refused(&fixture);
    assert!(!fixture.moved.join(RESET_INCOMPLETE).exists());
}

#[test]
fn reset_rechecks_identity_immediately_before_root_rename() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = swap_attack_fixture(dir.path());
    let plan = ResetPlan::preflight(&fixture.root, None).unwrap();
    let mut tx = ResetTransaction::new(plan, &mut |_, _| Ok(())).unwrap();
    let failure = tx
        .stage(&mut |_| Ok(()), &mut |stage, _| {
            if stage == ResetStage::RootReserveParent {
                swap_decoy_in(&fixture);
            }
            Ok(())
        })
        .err()
        .unwrap();
    assert!(matches!(failure.error, Error::Storage(_)));
    assert_swap_refused(&fixture);
}

#[test]
fn reset_marker_barrier_failure_leaves_both_original_names_and_no_tombstones() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let wal = dir.path().join("external");
    drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap());
    std::fs::write(root.join("old-root"), b"old root").unwrap();
    std::fs::write(wal.join("old-wal"), b"old WAL").unwrap();
    let plan = ResetPlan::preflight(&root, Some(&wal)).unwrap();
    let result = ResetTransaction::new(plan, &mut |stage, _| {
        assert_eq!(stage, ResetStage::MarkerDir);
        Err(Error::io("injected marker sync failure"))
    });
    assert!(matches!(result, Err(Error::Io(_))));
    assert_eq!(std::fs::read(root.join("old-root")).unwrap(), b"old root");
    assert_eq!(std::fs::read(wal.join("old-wal")).unwrap(), b"old WAL");
    assert!(root.join(RESET_INCOMPLETE).exists());
    assert_no_reset_residue(dir.path());
}

#[test]
fn reset_move_outcomes_match_each_barrier_and_preserve_old_bytes() {
    for failed in [
        ResetStage::WalReserveParent,
        ResetStage::WalOldParent,
        ResetStage::WalTombstone,
        ResetStage::RootReserveParent,
        ResetStage::RootOldParent,
        ResetStage::RootTombstone,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let wal = dir.path().join("external");
        drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap());
        std::fs::write(root.join("old-root"), b"old root").unwrap();
        std::fs::write(wal.join("old-wal"), b"old WAL").unwrap();
        let plan = ResetPlan::preflight(&root, Some(&wal)).unwrap();
        assert!(plan.moves == ResetMoves::WalThenRoot);
        let mut transaction = ResetTransaction::new(plan, &mut |_, _| Ok(())).unwrap();
        let Err(failure) = transaction.stage(&mut |_| Ok(()), &mut |stage, _| {
            if stage == failed {
                Err(Error::io("injected sync failure"))
            } else {
                Ok(())
            }
        }) else {
            panic!("did not reach {failed:?}");
        };
        let wal_moved = !matches!(failed, ResetStage::WalReserveParent);
        let root_moved = matches!(
            failed,
            ResetStage::RootOldParent | ResetStage::RootTombstone
        );
        assert!(
            matches!(
                (&transaction.wal, failed),
                (
                    MoveOutcome::NotMoved { reserved: Some(_) },
                    ResetStage::WalReserveParent
                ) | (
                    MoveOutcome::MovedNotDurable { .. },
                    ResetStage::WalOldParent | ResetStage::WalTombstone
                ) | (
                    MoveOutcome::Durable { .. },
                    ResetStage::RootReserveParent
                        | ResetStage::RootOldParent
                        | ResetStage::RootTombstone
                )
            ),
            "{failed:?}: unexpected WAL stage"
        );
        assert!(
            matches!(
                (&transaction.root, failed),
                (
                    MoveOutcome::NotMoved { reserved: None },
                    ResetStage::WalReserveParent
                        | ResetStage::WalOldParent
                        | ResetStage::WalTombstone
                ) | (
                    MoveOutcome::NotMoved { reserved: Some(_) },
                    ResetStage::RootReserveParent
                ) | (
                    MoveOutcome::MovedNotDurable { .. },
                    ResetStage::RootOldParent | ResetStage::RootTombstone
                )
            ),
            "{failed:?}: unexpected root stage"
        );
        assert_eq!(root.exists(), !root_moved, "{failed:?}");
        assert_eq!(wal.exists(), !wal_moved, "{failed:?}");
        let old_root = if root_moved {
            transaction.root.tombstone().unwrap().join("old-root")
        } else {
            root.join("old-root")
        };
        let old_wal = if wal_moved {
            transaction.wal.tombstone().unwrap().join("old-wal")
        } else {
            wal.join("old-wal")
        };
        assert_eq!(std::fs::read(old_root).unwrap(), b"old root", "{failed:?}");
        assert_eq!(std::fs::read(old_wal).unwrap(), b"old WAL", "{failed:?}");
        let marker = if root_moved {
            transaction.root.tombstone().unwrap().join(RESET_INCOMPLETE)
        } else {
            root.join(RESET_INCOMPLETE)
        };
        assert!(marker.exists(), "{failed:?}");
        let error = transaction.rollback(failure);
        assert!(
            error.to_string().contains("reset barrier failed"),
            "{failed:?}: {error}"
        );
        assert_eq!(
            root.exists(),
            !root_moved,
            "{failed:?}: must not rollback uncertain sync"
        );
        assert_eq!(
            wal.exists(),
            !wal_moved,
            "{failed:?}: must not rollback uncertain sync"
        );
    }
}

#[test]
fn reset_durable_stage_failure_restores_root_before_wal() {
    for failed in [MoveKind::Wal, MoveKind::Root] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let wal = dir.path().join("external");
        drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap());
        std::fs::write(root.join("old-root"), b"old root").unwrap();
        std::fs::write(wal.join("old-wal"), b"old WAL").unwrap();
        let plan = ResetPlan::preflight(&root, Some(&wal)).unwrap();
        let mut transaction = ResetTransaction::new(plan, &mut |_, _| Ok(())).unwrap();
        let Err(failure) = transaction.stage(
            &mut |stage| {
                if stage == failed {
                    Err(Error::io("injected staged failure"))
                } else {
                    Ok(())
                }
            },
            &mut |_, _| Ok(()),
        ) else {
            panic!("did not reach {failed:?}");
        };
        assert!(matches!(transaction.wal, MoveOutcome::Durable { .. }));
        assert!(matches!(
            (&transaction.root, failed),
            (MoveOutcome::NotMoved { .. }, MoveKind::Wal)
                | (MoveOutcome::Durable { .. }, MoveKind::Root)
        ));
        assert!(!wal.exists());
        assert_eq!(root.exists(), failed == MoveKind::Wal);
        let error = transaction.rollback(failure);
        assert!(error.to_string().contains("injected staged failure"));
        assert_eq!(std::fs::read(root.join("old-root")).unwrap(), b"old root");
        assert_eq!(std::fs::read(wal.join("old-wal")).unwrap(), b"old WAL");
        assert!(!root.join(RESET_INCOMPLETE).exists());
        assert_no_reset_residue(dir.path());
    }
}
