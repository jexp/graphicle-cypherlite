//! The reset-refusal arc of the native-ownership tests: marker-state
//! refusals, nested-owner and case-folding refusals, and the overlap
//! admission — extracted from `tests.rs` (the no-1k-line rule,
//! LABS-311). Shares its fixtures and helpers through `use super::*`.

use super::*;

#[test]
#[cfg(unix)]
fn reset_sync_failure_strands_old_wal_and_blocks_old_root() {
    for failed in [
        ResetStage::MarkerDir,
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
        let target = parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap();
        target
            .wal_vfs()
            .0
            .open("old", VfsOpen::create_new())
            .unwrap()
            .write_all(b"old WAL")
            .unwrap();
        drop(target);
        let mut stages = Vec::new();
        let err = reset_native_with_barrier(
            &root,
            Some(&wal),
            |_| Ok(()),
            |stage, _| {
                stages.push(stage);
                if stage == failed {
                    Err(Error::io("injected fsync failure"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert!(stages.contains(&failed), "{failed:?}: {err}");
        // Every injected stage's failure carries reset context: the
        // marker-dir barrier fails through `marker_remains` ("reset
        // marker … remains after sync failure"), every move stage through
        // the reset rollback text.
        assert!(err.to_string().contains("reset"), "{err}");
        assert!(
            root.join(RESET_INCOMPLETE).exists() || !root.exists(),
            "{failed:?}"
        );
        if root.exists() {
            assert!(
                matches!(
                    parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())),
                    Err(Error::Storage(_)) | Err(Error::Locked(_))
                ),
                "{failed:?}"
            );
        } else {
            let fresh = parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap();
            assert!(
                !wal.join("old").exists(),
                "{failed:?}: old WAL attached to fresh root"
            );
            drop(fresh);
        }
        let old_wal = if wal.join("old").exists() {
            wal.join("old")
        } else {
            std::fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().path().join("data/old"))
                .find(|path| path.exists())
                .expect("old WAL retained")
        };
        assert_eq!(std::fs::read(old_wal).unwrap(), b"old WAL", "{failed:?}");
    }
}

#[test]
#[cfg(unix)]
fn reset_of_root_blocked_by_incomplete_marker_reports_recovery_contract() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let wal = dir.path().join("external-wal");
    drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap());
    // A marker left by a previous incomplete reset blocks the old root.
    std::fs::write(root.join(RESET_INCOMPLETE), b"").unwrap();
    let error = reset_uri(root.to_str().unwrap(), Some(&wal)).unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    // The planner canonicalizes the marker's directory; canonicalize the
    // test's own spelling so the substring check is not defeated by the
    // platform's symlinked temp directory (macOS /var, Windows \\?\).
    let marker = std::fs::canonicalize(&root).unwrap().join(RESET_INCOMPLETE);
    assert!(
        error.to_string().contains(&marker.display().to_string()),
        "{error}"
    );
    assert!(error.to_string().contains("recover the old WAL"), "{error}");
    // The refusal is clean: the marker and both trees stay untouched.
    assert!(root.join(RESET_INCOMPLETE).exists());
    assert!(root.join(LOCK_MARKER).is_file());
    assert!(wal.join(LOCK_MARKER).is_file());
}

#[test]
#[cfg(unix)]
fn reset_durable_wal_barrier_precedes_root_move_and_deletion() {
    use std::cell::RefCell;
    #[derive(Debug, PartialEq, Eq)]
    enum Step {
        Barrier(ResetStage),
        Staged(MoveKind),
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let wal = dir.path().join("external");
    drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap());
    let order = RefCell::new(Vec::new());
    reset_native_with_barrier(
        &root,
        Some(&wal),
        |step| {
            order.borrow_mut().push(Step::Staged(step));
            Ok(())
        },
        |stage, _| {
            order.borrow_mut().push(Step::Barrier(stage));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        *order.borrow(),
        [
            Step::Barrier(ResetStage::MarkerDir),
            Step::Barrier(ResetStage::WalReserveParent),
            Step::Barrier(ResetStage::WalOldParent),
            Step::Barrier(ResetStage::WalTombstone),
            Step::Staged(MoveKind::Wal),
            Step::Barrier(ResetStage::RootReserveParent),
            Step::Barrier(ResetStage::RootOldParent),
            Step::Barrier(ResetStage::RootTombstone),
            Step::Staged(MoveKind::Root),
        ]
    );
    assert!(!root.exists());
    assert!(!wal.exists());
}

#[test]
fn reset_uri_removes_filesystem_db() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let (g, mut wal) = person_ada();
    let target = parse_uri(uri).unwrap();
    checkpoint_with_lsn(&target, &g, wal.lsn, None).unwrap();
    let lsn = wal.rotate().unwrap();
    wal.recycle(lsn).unwrap();
    assert!(dir.path().join("manifest.json").exists());
    drop(target);
    reset_uri(uri, None).unwrap();
    assert!(!dir.path().exists());
    reset_uri(uri, None).unwrap();
}

#[test]
fn reset_refuses_ancestor_wal_without_touching_live_sibling_or_root() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("databases");
    let root = parent.join("root");
    let sibling = parent.join("sibling");
    let target = parse_uri(root.to_str().unwrap()).unwrap();
    target
        .vfs()
        .open("wal/root.log", VfsOpen::create_new())
        .unwrap()
        .write_all(b"root WAL")
        .unwrap();
    drop(target);
    let live_sibling = parse_uri(sibling.to_str().unwrap()).unwrap();
    live_sibling
        .vfs()
        .open("wal/sibling.log", VfsOpen::create_new())
        .unwrap()
        .write_all(b"sibling WAL")
        .unwrap();
    let names = |path: &Path| {
        let mut names: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let before = (names(&parent), names(&root), names(&sibling));
    // The WAL override is an ancestor of root, while the independently
    // leased sibling lives under it. No ancestor marker is created.
    let alias = parent.join(".");
    let error = reset_uri(root.to_str().unwrap(), Some(&alias)).unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert_eq!(before, (names(&parent), names(&root), names(&sibling)));
    assert!(!parent.join(LOCK_MARKER).exists());
    assert!(!root.join(RESET_INCOMPLETE).exists());
    assert_eq!(
        std::fs::read(root.join("wal/root.log")).unwrap(),
        b"root WAL"
    );
    assert_eq!(
        std::fs::read(sibling.join("wal/sibling.log")).unwrap(),
        b"sibling WAL"
    );
    assert!(matches!(
        parse_uri(sibling.to_str().unwrap()),
        Err(Error::Locked(_))
    ));
    drop(live_sibling);
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    drop(parse_uri(sibling.to_str().unwrap()).unwrap());
}

#[test]
fn nested_native_and_bundle_open_refused_with_live_or_closed_parent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let child = root.join("data").join("child");
    let bundle = root.join("data").join("child.CL");
    let mut parent = Some(parse_uri(root.to_str().unwrap()).unwrap());
    for live in [true, false] {
        if !live {
            drop(parent.take());
        }
        for result in [
            parse_uri(child.to_str().unwrap()),
            parse_uri(bundle.to_str().unwrap()),
        ] {
            assert!(matches!(result, Err(Error::Storage(_))));
        }
        assert!(!child.exists());
        assert!(!bundle.exists());
        assert!(!root.join("data").exists());
    }
}

#[test]
fn opening_new_parent_over_existing_child_refuses_before_creating_marker() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("container");
    let child = root.join("child");
    let owner = parse_uri(child.to_str().unwrap()).unwrap();
    assert!(!root.join(LOCK_MARKER).exists());
    let err = parse_uri(root.to_str().unwrap()).err().unwrap();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert!(!root.join(LOCK_MARKER).exists());
    assert!(matches!(
        parse_uri(child.to_str().unwrap()),
        Err(Error::Locked(_))
    ));
    drop(owner);
    drop(parse_uri(child.to_str().unwrap()).unwrap());
}

#[test]
fn wal_scan_rejects_nested_owner_under_case_variant_of_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Graph");
    let wal = dir.path().join("graph");
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    // On a case-sensitive macOS/Windows volume these are different trees,
    // but the reset overlap predicate still calls the WAL contained.
    // On a case-insensitive volume the spellings alias the same tree;
    // that path also tests the post-lock scan with root/wal/.cypherlite.lock present.
    #[cfg(any(target_os = "macos", windows))]
    assert!(reset_path_starts_with(&wal, &root).unwrap());
    drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap());

    // Simulate a legacy writer which created a child before the WAL was
    // attached. A normal child open would reject its parent's marker.
    let child = wal.join("legacy").join("child");
    let (_, owner) = NativeVfs::lock_directory(&child, LOCK_MARKER).unwrap();
    std::fs::write(child.join("important"), b"independent database").unwrap();
    let error = parse_uri_with_wal(root.to_str().unwrap(), Some(wal))
        .err()
        .unwrap();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert!(error.to_string().contains("legacy"), "{error}");
    assert_eq!(
        std::fs::read(child.join("important")).unwrap(),
        b"independent database"
    );
    assert!(matches!(
        NativeVfs::lock_directory(&child, LOCK_MARKER),
        Err(Error::Locked(_))
    ));
    drop(owner);
}

#[test]
#[cfg(unix)]
fn nested_aliases_follow_physical_identity_without_following_outbound_links_on_reset() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    let alias = dir.path().join("db-alias");
    symlink(&root, &alias).unwrap();
    let child = alias.join("nested");
    assert!(matches!(
        parse_uri(child.to_str().unwrap()),
        Err(Error::Storage(_))
    ));

    let external = dir.path().join("external.cl");
    drop(BundleVfs::open_or_create(&external).unwrap());
    symlink(&external, root.join("outbound.cl")).unwrap();
    // The actual host file is outside the root; reset only removes the
    // symlink and leaves the independent external bundle unchanged.
    let owner = BundleVfs::open_or_create(&external).unwrap();
    reset_uri(root.to_str().unwrap(), None).unwrap();
    assert!(external.exists());
    assert!(matches!(
        BundleVfs::open_or_create(&external),
        Err(Error::Locked(_))
    ));
    drop(owner);
}

#[test]
fn reset_refuses_preexisting_nested_native_owner_without_any_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    let child = root.join("legacy").join("nested");
    // Fixture bypasses the URI policy, as a legacy/external writer could.
    let (_, owner) = NativeVfs::lock_directory(&child, LOCK_MARKER).unwrap();
    std::fs::write(child.join("important"), b"independent native data").unwrap();
    let before = std::fs::read(root.join(LOCK_MARKER)).unwrap();
    let err = reset_uri(root.to_str().unwrap(), None).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert_eq!(std::fs::read(root.join(LOCK_MARKER)).unwrap(), before);
    assert_eq!(
        std::fs::read(child.join("important")).unwrap(),
        b"independent native data"
    );
    assert!(!root.join(RESET_INCOMPLETE).exists());
    assert_no_reset_residue(dir.path());
    assert!(matches!(
        NativeVfs::lock_directory(&child, LOCK_MARKER),
        Err(Error::Locked(_))
    ));
    drop(owner);
    assert!(child.join(LOCK_MARKER).exists());
}

#[test]
fn reset_refuses_nested_uppercase_lock_marker_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    let child = root.join("legacy");
    std::fs::create_dir(&child).unwrap();
    std::fs::write(
        child.join(LOCK_MARKER.to_ascii_uppercase()),
        b"nested owner",
    )
    .unwrap();
    std::fs::write(child.join("important"), b"preserve").unwrap();
    let error = reset_uri(root.to_str().unwrap(), None).unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert_eq!(std::fs::read(child.join("important")).unwrap(), b"preserve");
    assert_eq!(
        std::fs::read(child.join(LOCK_MARKER.to_ascii_uppercase())).unwrap(),
        b"nested owner"
    );
    assert!(root.join(LOCK_MARKER).exists());
    assert!(!root.join(RESET_INCOMPLETE).exists());
    assert_no_reset_residue(dir.path());
}

#[test]
fn reset_distinguishes_extra_uppercase_marker_from_own_marker() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    // A case-sensitive volume can hold both spellings. The uppercase
    // marker is not the root's own lock merely because its name folds.
    let extra = root.join(LOCK_MARKER.to_ascii_uppercase());
    if !extra.exists() {
        std::fs::write(&extra, b"other marker").unwrap();
        let error = reset_uri(root.to_str().unwrap(), None).unwrap_err();
        assert!(matches!(error, Error::Storage(_)), "{error}");
        assert_eq!(std::fs::read(&extra).unwrap(), b"other marker");
        assert!(root.join(LOCK_MARKER).exists());
    }
}

#[test]
fn reset_markerless_parent_treats_direct_wal_lock_as_independent_child() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("container");
    let child = root.join("wal");
    let (_, owner) = NativeVfs::lock_directory(&child, LOCK_MARKER).unwrap();
    std::fs::write(child.join("important"), b"independent wal-named database").unwrap();
    assert!(!root.join(LOCK_MARKER).exists());
    let err = reset_uri(root.to_str().unwrap(), None).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert!(!root.join(LOCK_MARKER).exists());
    assert_eq!(
        std::fs::read(child.join("important")).unwrap(),
        b"independent wal-named database"
    );
    assert_no_reset_residue(dir.path());
    drop(owner);
}

#[test]
fn reset_refuses_preexisting_live_bundle_without_any_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    let bundle = root.join("legacy").join("nested.CL");
    // A valid legacy bundle, installed without going through URI parsing.
    let seed = dir.path().join("seed.cl");
    drop(BundleVfs::open_or_create(&seed).unwrap());
    std::fs::create_dir_all(bundle.parent().unwrap()).unwrap();
    std::fs::copy(seed, &bundle).unwrap();
    let owner = NativeVfs::lock_regular_file(&bundle).unwrap();
    let before = read_all_locked(&owner, &bundle);
    let err = reset_uri(root.to_str().unwrap(), None).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert_eq!(read_all_locked(&owner, &bundle), before);
    assert!(!root.join(RESET_INCOMPLETE).exists());
    assert_no_reset_residue(dir.path());
    assert!(matches!(
        NativeVfs::lock_regular_file(&bundle),
        Err(Error::Locked(_))
    ));
    drop(owner);
    assert_eq!(std::fs::read(&bundle).unwrap(), before);
}

#[test]
#[cfg(unix)]
fn reset_recognizes_internal_cl_alias_to_non_cl_bundle_host() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    let seed = dir.path().join("seed.cl");
    drop(BundleVfs::open_or_create(&seed).unwrap());
    let host = root.join("payload.bin");
    std::fs::copy(&seed, &host).unwrap();
    let alias = root.join("alias.cl");
    symlink(&host, &alias).unwrap();
    let owner = NativeVfs::lock_regular_file(&host).unwrap();
    let before = std::fs::read(&host).unwrap();
    let err = reset_uri(root.to_str().unwrap(), None).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert_eq!(std::fs::read(&host).unwrap(), before);
    assert!(alias.exists());
    assert!(!root.join(RESET_INCOMPLETE).exists());
    assert_no_reset_residue(dir.path());
    drop(owner);
}

#[test]
fn reset_absent_root_refuses_ancestor_override_before_creating_any_lock() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    let root = parent.join("not-created");
    let sibling = parent.join("sibling");
    let live = parse_uri(sibling.to_str().unwrap()).unwrap();
    let error = reset_uri(root.to_str().unwrap(), Some(&parent)).unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert!(!root.exists());
    assert!(!parent.join(LOCK_MARKER).exists());
    assert_no_reset_residue(&parent);
    assert!(sibling.join("wal").join(LOCK_MARKER).exists());
    drop(live);
}

#[test]
#[cfg(any(target_os = "macos", windows))]
fn reset_rejects_case_alias_ancestor_even_when_paths_are_missing() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("CaseParent");
    std::fs::create_dir(&parent).unwrap();
    let alias = dir.path().join("caseparent");
    // macOS volumes can be case-sensitive; only exercise the physical
    // alias regression when this temp directory is case-insensitive.
    if !alias.exists() {
        return;
    }
    let sibling = parent.join("sibling");
    let live = parse_uri(sibling.to_str().unwrap()).unwrap();
    let root = parent.join("db");
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    std::fs::write(root.join("important"), b"root").unwrap();
    for path in [&root, &parent.join("absent")] {
        let error = reset_uri(path.to_str().unwrap(), Some(&alias)).unwrap_err();
        assert!(matches!(error, Error::Storage(_)), "{error}");
    }
    // Both paths below are absent; their unresolved components can
    // still alias on this volume. Check before any lock creation.
    let missing_root = parent.join("New").join("db");
    let missing_wal = parent.join("new");
    let error = reset_uri(missing_root.to_str().unwrap(), Some(&missing_wal)).unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert!(!parent.join("New").exists());
    assert!(!parent.join("absent").exists());
    assert!(!parent.join(LOCK_MARKER).exists());
    assert!(!root.join(RESET_INCOMPLETE).exists());
    assert_eq!(std::fs::read(root.join("important")).unwrap(), b"root");
    assert!(matches!(
        parse_uri(sibling.to_str().unwrap()),
        Err(Error::Locked(_))
    ));
    drop(live);
}

#[test]
fn reset_case_sensitive_sibling_wal_is_not_direct_wal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    let alternate = root.join("WAL");
    if !alternate.exists() {
        std::fs::create_dir(&alternate).unwrap();
        let error = reset_uri(root.to_str().unwrap(), Some(&alternate)).unwrap_err();
        assert!(matches!(error, Error::Storage(_)), "{error}");
        assert!(!alternate.join(LOCK_MARKER).exists());
        assert!(root.join("wal").join(LOCK_MARKER).exists());
    }
}

#[test]
fn reset_missing_unicode_components_fail_closed_on_casefolding_platforms() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("caf\u{e9}").join("db");
    let wal = dir.path().join("cafe\u{301}");
    let overlap = validate_reset_overlap(&root, &wal);
    #[cfg(any(target_os = "macos", windows))]
    assert!(matches!(overlap, Err(Error::Storage(_))));
    #[cfg(not(any(target_os = "macos", windows)))]
    assert!(overlap.is_ok());
    assert!(!root.exists());
    assert!(!wal.exists());
}

#[test]
#[cfg(any(target_os = "macos", windows))]
fn reset_accepts_physical_direct_wal_with_different_casing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    let upper_wal = root.join("WAL");
    if !upper_wal.exists() {
        // On a case-sensitive volume this is an independent directory,
        // not the default WAL identity.
        return;
    }
    if !root.join("wal").exists() {
        return;
    }
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    #[cfg(target_os = "macos")]
    {
        std::fs::rename(
            root.join(LOCK_MARKER),
            root.join(LOCK_MARKER.to_ascii_uppercase()),
        )
        .unwrap();
        std::fs::rename(
            upper_wal.join(LOCK_MARKER),
            upper_wal.join(LOCK_MARKER.to_ascii_uppercase()),
        )
        .unwrap();
    }
    std::fs::write(root.join("important"), b"owned root").unwrap();
    reset_uri(root.to_str().unwrap(), Some(&upper_wal)).unwrap();
    assert!(!root.exists());
}

#[test]
fn reset_overlap_rejects_equal_and_unsafe_descendants_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    // The equal spelling (`root/.`) aliases the root's own lock, so it is
    // refused as an overlap — a different refusal from a descendant's, and
    // asserted on its own so the descendant loop's no-marker claim below
    // stays unconditional.
    let error = reset_uri(root.to_str().unwrap(), Some(&root.join("."))).unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert!(root.join(LOCK_MARKER).exists());
    assert!(!root.join(RESET_INCOMPLETE).exists());
    for wal in [root.join("nested"), root.join("wal/nested")] {
        let error = reset_uri(root.to_str().unwrap(), Some(&wal)).unwrap_err();
        assert!(matches!(error, Error::Storage(_)), "{error}");
        assert!(root.join(LOCK_MARKER).exists());
        assert!(!wal.join(LOCK_MARKER).exists());
        assert!(!root.join(RESET_INCOMPLETE).exists());
    }
    assert!(root.join("wal").is_dir());
    reset_uri(root.to_str().unwrap(), Some(&root.join("wal"))).unwrap();
    assert!(!root.exists());
}

#[test]
#[cfg(unix)]
fn reset_rejects_symlink_aliases_to_ancestor_and_unsafe_descendant() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    let root = parent.join("db");
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    let alias = dir.path().join("alias");
    symlink(&parent, &alias).unwrap();
    let root_alias = alias.join("db");
    for wal in [&alias, &root_alias.join("wal/other")] {
        let error = reset_uri(root_alias.to_str().unwrap(), Some(wal)).unwrap_err();
        assert!(matches!(error, Error::Storage(_)), "{error}");
    }
    assert!(!parent.join(LOCK_MARKER).exists());
    assert!(!root.join("wal/other").exists());
    assert!(!root.join(RESET_INCOMPLETE).exists());
    assert_no_reset_residue(&parent);
    drop(parse_uri(root.to_str().unwrap()).unwrap());
}
