use super::identity::validate_reset_overlap;
// `reset_path_starts_with` is only exercised by macOS/Windows case-insensitive
// filesystem tests, so gate the import with the same cfg as its use.
#[cfg(any(target_os = "macos", windows))]
use super::identity::reset_path_starts_with;
use super::reset::rename_reset_noreplace;
#[cfg(unix)]
use super::reset::reset_native_with_barrier;
use super::*;
use crate::tests::person_ada;
use crate::{
    adopt_native_default_wal, checkpoint_with_lsn, load, open_store, parse_uri, parse_uri_with_wal,
    reset_uri, BundleVfs, VfsOpen, Wal,
};
use std::io::{Read, Seek};
use std::sync::Arc;

/// Read a marker through the handle that already owns its byte-range lock.
/// Opening a fresh handle to a locked region fails with `os error 33` on
/// Windows, so a locked marker must be read via its owning handle.
fn read_locked(owner: &std::fs::File, path: &std::path::Path) -> Vec<u8> {
    NativeVfs::read_locked_marker(owner, path).unwrap()
}

/// Read the entire locked file through a duplicate of its owning handle.
/// Used where the bytes are larger than the 128-byte marker cap.
fn read_all_locked(owner: &std::fs::File, path: &std::path::Path) -> Vec<u8> {
    NativeVfs::check_identity(owner, path).unwrap();
    let mut file = owner.try_clone().unwrap();
    file.seek(std::io::SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    bytes
}

#[test]
fn readonly_open_rejects_a_wal_marker_as_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let wal = dir.path().join("not-a-root");
    std::fs::create_dir(&wal).unwrap();
    std::fs::write(wal.join(LOCK_MARKER), WAL_ROLE).unwrap();
    let err = match lease_readonly(&wal, &wal) {
        Err(err) => err,
        Ok(_) => panic!("read-only open accepted a WAL marker as the root"),
    };
    assert!(
        err.to_string().contains("wrong or malformed lock role"),
        "{err}"
    );
}

#[test]
fn reset_role_markers_reject_closed_root_as_disjoint_wal_before_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("owner");
    let other = dir.path().join("other");
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    drop(parse_uri(other.to_str().unwrap()).unwrap());
    assert_eq!(std::fs::read(root.join(LOCK_MARKER)).unwrap(), ROOT_ROLE);
    assert_eq!(
        std::fs::read(root.join("wal").join(LOCK_MARKER)).unwrap(),
        WAL_ROLE
    );
    let before = std::fs::read(other.join(LOCK_MARKER)).unwrap();
    for result in [
        parse_uri_with_wal(root.to_str().unwrap(), Some(other.clone())).map(|_| ()),
        reset_uri(root.to_str().unwrap(), Some(&other)),
    ] {
        assert!(matches!(result, Err(Error::Storage(_))), "{result:?}");
    }
    assert_eq!(std::fs::read(other.join(LOCK_MARKER)).unwrap(), before);
    assert!(!root.join(RESET_INCOMPLETE).exists());
    assert!(other.join("wal").join(LOCK_MARKER).exists());
}

#[test]
fn reset_refuses_ambiguous_empty_legacy_wal_without_marking_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let wal = dir.path().join("legacy-wal");
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    std::fs::create_dir(&wal).unwrap();
    std::fs::write(wal.join(LOCK_MARKER), b"").unwrap();
    for result in [
        parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).map(|_| ()),
        reset_uri(root.to_str().unwrap(), Some(&wal)),
    ] {
        assert!(matches!(result, Err(Error::Storage(_))), "{result:?}");
    }
    assert_eq!(std::fs::read(wal.join(LOCK_MARKER)).unwrap(), b"");
    assert!(!root.join(RESET_INCOMPLETE).exists());
    assert!(root.exists());
}

#[test]
fn offline_adoption_recovers_pre_adr_snapshot_and_wal_without_lock_files() {
    // Captured from origin/main (615306d), before ADR-0097 introduced
    // typed marker files. Ada is checkpointed at LSN 1; Bob is WAL-only.
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pre_adr0084_default_wal");
    let names = |path: &Path| {
        let mut names: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    };
    assert_eq!(names(&fixture), ["graph.json", "manifest.json", "wal"]);
    assert_eq!(names(&fixture.join("wal")), ["00000001.log"]);
    assert!(!fixture.join(LOCK_MARKER).exists());
    assert!(!fixture.join("wal").join(LOCK_MARKER).exists());
    let manifest = std::fs::read(fixture.join("manifest.json")).unwrap();
    let snapshot = std::fs::read(fixture.join("graph.json")).unwrap();
    let frames = std::fs::read(fixture.join("wal/00000001.log")).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&manifest).unwrap(),
        serde_json::json!({"codec": "json", "checkpoint_lsn": 1, "commit_ts": 1})
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&snapshot).unwrap()["nodes"],
        serde_json::json!([{"id": 0, "labels": ["Person"],
                "properties": {"age": 37, "name": "Ada"}}])
    );
    assert_eq!(&frames[..8], crate::wal::MAGIC);
    let mut offset = 8;
    for (lsn, id, name, age) in [(1, 0, "Ada", 37), (2, 1, "Bob", 42)] {
        let crc = u32::from_le_bytes(frames[offset..offset + 4].try_into().unwrap());
        let len = u32::from_le_bytes(frames[offset + 4..offset + 8].try_into().unwrap());
        offset += 8;
        let payload = &frames[offset..offset + len as usize];
        assert_eq!(crc32fast::hash(payload), crc, "legacy WAL frame CRC");
        let record: serde_json::Value = serde_json::from_slice(payload).unwrap();
        assert_eq!(
            record,
            serde_json::json!({"lsn": lsn, "commit_ts": lsn,
                    "ops": [{"op": "CreateNode", "id": id, "labels": ["Person"],
                        "properties": {"age": {"t": "int", "v": age},
                            "name": {"t": "string", "v": name}}}]})
        );
        offset += len as usize;
    }
    assert_eq!(offset, frames.len(), "no unverified WAL tail");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("legacy");
    let wal_path = root.join("wal/00000001.log");
    std::fs::create_dir_all(root.join("wal")).unwrap();
    std::fs::write(root.join("manifest.json"), &manifest).unwrap();
    std::fs::write(root.join("graph.json"), &snapshot).unwrap();
    std::fs::write(&wal_path, &frames).unwrap();

    for result in [
        parse_uri(root.to_str().unwrap()).map(|_| ()),
        reset_uri(root.to_str().unwrap(), None),
    ] {
        assert!(matches!(result, Err(Error::Storage(_))), "{result:?}");
    }
    assert!(!root.join(LOCK_MARKER).exists());
    assert!(!root.join("wal").join(LOCK_MARKER).exists());
    assert_eq!(std::fs::read(root.join("manifest.json")).unwrap(), manifest);
    assert_eq!(std::fs::read(root.join("graph.json")).unwrap(), snapshot);
    assert_eq!(std::fs::read(&wal_path).unwrap(), frames);

    adopt_native_default_wal(&root).unwrap();
    assert_eq!(std::fs::read(root.join(LOCK_MARKER)).unwrap(), ROOT_ROLE);
    assert_eq!(
        std::fs::read(root.join("wal").join(LOCK_MARKER)).unwrap(),
        WAL_ROLE
    );
    assert_eq!(std::fs::read(root.join("manifest.json")).unwrap(), manifest);
    assert_eq!(std::fs::read(root.join("graph.json")).unwrap(), snapshot);
    assert_eq!(std::fs::read(&wal_path).unwrap(), frames);
    adopt_native_default_wal(&root).unwrap(); // stable, idempotent roles
    let target = parse_uri(root.to_str().unwrap()).unwrap();
    let (restored, replayed) = open_store(&target).unwrap();
    assert_eq!(replayed.lsn, 2);
    // ADR-0127: the legacy store (no `value_encoding` marker) migrated on
    // open — checkpointed at the current format, WAL rotated to a fresh
    // segment with the current magic and the fully checkpointed segment
    // recycled.
    assert_eq!(replayed.bytes, crate::wal::MAGIC.len() as u64);
    assert!(replayed.replay(0).unwrap().is_empty());
    assert_eq!(restored.live_node_count(), 2);
    assert_eq!(
        restored.node(0).unwrap(),
        cypherlite_core::Node {
            id: 0,
            labels: vec!["Person".into()].into(),
            properties: cypherlite_core::PropertyMap::share(std::collections::BTreeMap::from([
                ("age".into(), cypherlite_core::Value::Int(37)),
                ("name".into(), cypherlite_core::Value::String("Ada".into())),
            ])),
        }
    );
    assert_eq!(
        restored.node(1).unwrap(),
        cypherlite_core::Node {
            id: 1,
            labels: vec!["Person".into()].into(),
            properties: cypherlite_core::PropertyMap::share(std::collections::BTreeMap::from([
                ("age".into(), cypherlite_core::Value::Int(42)),
                ("name".into(), cypherlite_core::Value::String("Bob".into())),
            ])),
        }
    );
    drop((restored, replayed, target));
    // Migration rewrote the snapshot at the current format: graph.json
    // carries `value_encoding` 2 and the WAL is a single fresh segment.
    let migrated: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("graph.json")).unwrap()).unwrap();
    assert_eq!(migrated["value_encoding"], 2);
    assert_eq!(migrated["nodes"].as_array().unwrap().len(), 2);
    assert!(!wal_path.exists());
    assert_eq!(
        std::fs::read(root.join("wal/00000002.log")).unwrap(),
        crate::wal::MAGIC
    );
    reset_uri(root.to_str().unwrap(), None).unwrap();
    assert!(!root.exists());
}

#[test]
fn offline_adoption_checks_ownership_evidence_and_other_nested_owners() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("outer");
    let wal = root.join("wal");
    std::fs::create_dir_all(&wal).unwrap();
    std::fs::write(root.join("manifest.json"), b"root evidence").unwrap();
    std::fs::write(wal.join("00000001.log"), crate::wal::MAGIC).unwrap();
    // This layout is indistinguishable from a legacy nested root==WAL.
    // Ordinary open/reset must not infer it belongs to the outer root.
    for result in [
        parse_uri(root.to_str().unwrap()).map(|_| ()),
        reset_uri(root.to_str().unwrap(), None),
    ] {
        assert!(matches!(result, Err(Error::Storage(_))), "{result:?}");
    }
    assert!(!root.join(LOCK_MARKER).exists());
    assert!(!wal.join(LOCK_MARKER).exists());
    std::fs::write(wal.join("graph.json"), b"independent root").unwrap();
    assert!(matches!(
        adopt_native_default_wal(&root),
        Err(Error::Storage(_))
    ));
    assert!(!root.join(LOCK_MARKER).exists());
    assert!(!wal.join(LOCK_MARKER).exists());
    std::fs::remove_file(wal.join("graph.json")).unwrap();
    let child = wal.join("nested.cl");
    std::fs::write(&child, b"independent bundle").unwrap();
    assert!(matches!(
        adopt_native_default_wal(&root),
        Err(Error::Storage(_))
    ));
    assert!(!root.join(LOCK_MARKER).exists());
    assert!(!wal.join(LOCK_MARKER).exists());
    assert_eq!(std::fs::read(child).unwrap(), b"independent bundle");
}

#[test]
fn offline_adoption_refuses_live_lease_wrong_role_and_incomplete_reset() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("legacy");
    let wal = root.join("wal");
    std::fs::create_dir_all(&wal).unwrap();
    std::fs::write(root.join("manifest.json"), b"root evidence").unwrap();
    std::fs::write(wal.join("00000001.log"), crate::wal::MAGIC).unwrap();
    let (_, root_owner) = NativeVfs::lock_directory(&root).unwrap();
    std::fs::write(wal.join(LOCK_MARKER), b"").unwrap();
    let root_lock = root.join(LOCK_MARKER);
    assert!(matches!(
        adopt_native_default_wal(&root),
        Err(Error::Locked(_))
    ));
    assert_eq!(read_locked(&root_owner, &root_lock), b"");
    drop(root_owner);
    let (_, wal_owner) = NativeVfs::lock_directory(&wal).unwrap();
    assert!(matches!(
        adopt_native_default_wal(&root),
        Err(Error::Locked(_))
    ));
    assert_eq!(read_locked(&wal_owner, &wal.join(LOCK_MARKER)), b"");
    drop(wal_owner);

    std::fs::write(wal.join(LOCK_MARKER), ROOT_ROLE).unwrap();
    assert!(matches!(
        adopt_native_default_wal(&root),
        Err(Error::Storage(_))
    ));
    assert_eq!(std::fs::read(&root_lock).unwrap(), b"");
    std::fs::write(wal.join(LOCK_MARKER), b"").unwrap();
    std::fs::write(root.join(RESET_INCOMPLETE), b"blocked").unwrap();
    assert!(matches!(
        adopt_native_default_wal(&root),
        Err(Error::Storage(_))
    ));
    assert_eq!(std::fs::read(&root_lock).unwrap(), b"");
    assert_eq!(std::fs::read(wal.join(LOCK_MARKER)).unwrap(), b"");
}

#[test]
fn legacy_root_with_typed_direct_wal_opens_then_reset_migrates_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("legacy");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("manifest.json"), b"corrupt but root evidence").unwrap();
    std::fs::create_dir(root.join("wal")).unwrap();
    std::fs::write(root.join(LOCK_MARKER), b"").unwrap();
    std::fs::write(root.join("wal").join(LOCK_MARKER), WAL_ROLE).unwrap();
    let (resolved, wal, owner) = lease(&root, &root.join("wal")).unwrap();
    assert_eq!(read_locked(&owner._root, &resolved.join(LOCK_MARKER)), b"");
    assert_eq!(
        read_locked(owner._wal.as_ref().unwrap(), &wal.join(LOCK_MARKER)),
        WAL_ROLE
    );
    drop(owner);
    drop(lease(&root, &root.join("wal")).unwrap());
    assert!(parse_uri(root.to_str().unwrap()).is_err());
    assert_eq!(std::fs::read(root.join(LOCK_MARKER)).unwrap(), b"");
    assert_eq!(
        std::fs::read(root.join("wal").join(LOCK_MARKER)).unwrap(),
        WAL_ROLE
    );

    let missing = dir.path().join("pre-lock");
    std::fs::create_dir(&missing).unwrap();
    std::fs::write(missing.join("manifest.json"), b"root evidence").unwrap();
    std::fs::create_dir(missing.join("wal")).unwrap();
    std::fs::write(missing.join("wal").join(LOCK_MARKER), WAL_ROLE).unwrap();
    drop(lease(&missing, &missing.join("wal")).unwrap());
    assert_eq!(std::fs::read(missing.join(LOCK_MARKER)).unwrap(), b"");
    assert_eq!(
        std::fs::read(missing.join("wal").join(LOCK_MARKER)).unwrap(),
        WAL_ROLE
    );

    #[cfg(unix)]
    {
        let error = reset_native_with_barrier(
            &root,
            None,
            |_| Ok(()),
            |stage, _| {
                if stage == "root-reserve-parent" {
                    Err(Error::io("stop after role migration"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert!(matches!(error, Error::Storage(_)));
        assert_eq!(std::fs::read(root.join(LOCK_MARKER)).unwrap(), ROOT_ROLE);
        assert_eq!(
            std::fs::read(root.join("wal").join(LOCK_MARKER)).unwrap(),
            WAL_ROLE
        );
    }

    let same = dir.path().join("same");
    drop(parse_uri_with_wal(same.to_str().unwrap(), Some(same.clone())).unwrap());
    assert_eq!(std::fs::read(same.join(LOCK_MARKER)).unwrap(), ROOT_ROLE);
    assert!(matches!(
        reset_uri(same.to_str().unwrap(), Some(&same)),
        Err(Error::Storage(_))
    ));
}

#[test]
fn wal_segment_magic_evidence_admits_both_readable_magics() {
    // ADR-0120 §4 / ADR-0090: ownership admission and the WAL scanner share
    // the registry's one readable-magic classification (ADR-0127 Decision
    // 5), so an `MDBWAL02` segment is WAL evidence exactly like `MDBWAL01`,
    // and a still-future magic is not.
    for (magic, adopts) in [
        (crate::wal::MAGIC, true),
        (crate::version::wal::ONTOLOGY_MAGIC, true),
        (b"MDBWAL03", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("legacy");
        let wal = root.join("wal");
        std::fs::create_dir_all(&wal).unwrap();
        std::fs::write(root.join("manifest.json"), b"root evidence").unwrap();
        std::fs::write(wal.join("00000001.log"), magic).unwrap();
        let result = adopt_native_default_wal(&root);
        if adopts {
            result.unwrap();
        } else {
            assert!(matches!(result, Err(Error::Storage(_))), "{result:?}");
        }
    }
}

#[test]
fn segment_magic_cannot_upgrade_a_disjoint_untyped_wal_or_same_directory_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let wal = dir.path().join("untyped");
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    std::fs::create_dir(&wal).unwrap();
    std::fs::write(wal.join("00000001.log"), crate::wal::MAGIC).unwrap();
    for result in [
        parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).map(|_| ()),
        reset_uri(root.to_str().unwrap(), Some(&wal)),
    ] {
        assert!(matches!(result, Err(Error::Storage(_))), "{result:?}");
    }
    assert!(!wal.join(LOCK_MARKER).exists());
    assert_eq!(
        std::fs::read(wal.join("00000001.log")).unwrap(),
        crate::wal::MAGIC
    );
    assert!(matches!(lease(&wal, &wal), Err(Error::Storage(_))));
    assert!(!wal.join(LOCK_MARKER).exists());
}

#[test]
fn closed_legacy_root_at_direct_wal_is_never_claimed_by_outer_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("outer");
    let external = dir.path().join("external");
    drop(parse_uri_with_wal(root.to_str().unwrap(), Some(external.clone())).unwrap());
    let child = root.join("wal");
    std::fs::create_dir(&child).unwrap();
    let child_canonical = std::fs::canonicalize(&child).unwrap();
    std::fs::write(child.join(LOCK_MARKER), b"").unwrap();
    let mut segment = Wal::open(Arc::new(NativeVfs::new(&child)), "", 0).unwrap();
    segment.append_commit(7, &[]).unwrap();
    drop(segment);
    let segment_path = child.join("00000001.log");
    let original_segment = std::fs::read(&segment_path).unwrap();
    assert_eq!(&original_segment[..8], crate::wal::MAGIC);
    for (manifest, marked_root) in [(false, true), (true, true), (true, false)] {
        if manifest {
            std::fs::write(root.join("manifest.json"), b"outer root evidence").unwrap();
        }
        if !marked_root {
            std::fs::remove_file(root.join(LOCK_MARKER)).unwrap();
        }
        for (index, result) in [
            parse_uri_with_wal(root.to_str().unwrap(), Some(external.clone())).map(|_| ()),
            reset_uri(root.to_str().unwrap(), Some(&external)),
            parse_uri(root.to_str().unwrap()).map(|_| ()),
            reset_uri(root.to_str().unwrap(), None),
        ]
        .into_iter()
        .enumerate()
        {
            let error = result.unwrap_err();
            assert!(matches!(&error, Error::Storage(_)), "{error}");
            // On Windows an external-WAL reset is refused up front (no durable
            // directory rename barrier) before nested-owner detection, so its
            // message names that limitation rather than the nested child.
            if !(cfg!(windows) && index == 1) {
                assert!(
                    error
                        .to_string()
                        .contains(&child_canonical.display().to_string()),
                    "{error}"
                );
            }
            assert_eq!(std::fs::read(&segment_path).unwrap(), original_segment);
            assert_eq!(std::fs::read(child.join(LOCK_MARKER)).unwrap(), b"");
            if marked_root {
                assert_eq!(std::fs::read(root.join(LOCK_MARKER)).unwrap(), ROOT_ROLE);
            } else {
                assert!(!root.join(LOCK_MARKER).exists());
            }
            assert_eq!(std::fs::read(external.join(LOCK_MARKER)).unwrap(), WAL_ROLE);
            assert!(!root.join(RESET_INCOMPLETE).exists());
            assert!(!std::fs::read_dir(dir.path()).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".reset-")));
        }
    }
}

#[test]
fn markerless_legacy_root_with_typed_default_wal_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("legacy");
    let wal = root.join("wal");
    std::fs::create_dir_all(&wal).unwrap();
    std::fs::write(wal.join(LOCK_MARKER), WAL_ROLE).unwrap();
    let mut segment = Wal::open(Arc::new(NativeVfs::new(&wal)), "", 0).unwrap();
    segment.append_commit(7, &[]).unwrap();
    drop(segment);
    let target = parse_uri(root.to_str().unwrap()).unwrap();
    // The empty root marker is lock-protected; a fresh handle cannot read it on
    // Windows. Directory metadata is unaffected by the byte-range lock and
    // proves the marker is the empty (not a typed) role.
    assert_eq!(std::fs::metadata(root.join(LOCK_MARKER)).unwrap().len(), 0);
    let (_graph, replayed) = open_store(&target).unwrap();
    assert!(replayed.lsn > 0);
    drop((replayed, target));
    drop(parse_uri(root.to_str().unwrap()).unwrap());

    // The same directory as root and WAL needs its own root evidence;
    // a typed child WAL does not make this parent a root == WAL store.
    std::fs::remove_file(root.join(LOCK_MARKER)).unwrap();
    assert!(matches!(
        parse_uri_with_wal(root.to_str().unwrap(), Some(root.clone())),
        Err(Error::Storage(_))
    ));
    assert!(!root.join(LOCK_MARKER).exists());
    reset_uri(root.to_str().unwrap(), None).unwrap();
    assert!(!root.exists());
}

#[test]
fn explicit_root_equals_wal_reopens_with_root_role() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("shared");
    drop(parse_uri_with_wal(root.to_str().unwrap(), Some(root.clone())).unwrap());
    assert_eq!(std::fs::read(root.join(LOCK_MARKER)).unwrap(), ROOT_ROLE);
    let target = parse_uri_with_wal(root.to_str().unwrap(), Some(root.clone())).unwrap();
    assert_eq!(
        target.filesystem_dir().unwrap(),
        std::fs::canonicalize(root).unwrap()
    );
    drop(target);
}

#[test]
fn typed_wal_with_root_evidence_and_unknown_roles_fail_without_rewriting_marker() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let wal = dir.path().join("external");
    drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap());
    std::fs::write(wal.join("graph.json"), b"independent root evidence").unwrap();
    assert!(matches!(
        parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())),
        Err(Error::Storage(_))
    ));
    assert!(matches!(
        reset_uri(root.to_str().unwrap(), Some(&wal)),
        Err(Error::Storage(_))
    ));
    assert_eq!(std::fs::read(wal.join(LOCK_MARKER)).unwrap(), WAL_ROLE);
    std::fs::remove_file(wal.join("graph.json")).unwrap();
    std::fs::write(wal.join(LOCK_MARKER), b"unknown role").unwrap();
    assert!(matches!(
        parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())),
        Err(Error::Storage(_))
    ));
    assert!(matches!(
        reset_uri(root.to_str().unwrap(), Some(&wal)),
        Err(Error::Storage(_))
    ));
    assert_eq!(
        std::fs::read(wal.join(LOCK_MARKER)).unwrap(),
        b"unknown role"
    );
}

#[test]
#[cfg(unix)]
fn reset_sync_failure_strands_old_wal_and_blocks_old_root() {
    for failed in [
        "marker-dir",
        "wal-reserve-parent",
        "wal-old-parent",
        "wal-tombstone",
        "root-reserve-parent",
        "root-old-parent",
        "root-tombstone",
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
                stages.push(stage.to_string());
                if stage == failed {
                    Err(Error::io("injected fsync failure"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert!(
            stages.iter().any(|stage| stage == failed),
            "{failed}: {err}"
        );
        if failed != "marker-dir" {
            assert!(err.to_string().contains("reset"), "{err}");
        }
        assert!(
            root.join(RESET_INCOMPLETE).exists() || !root.exists(),
            "{failed}"
        );
        if root.exists() {
            assert!(
                matches!(
                    parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())),
                    Err(Error::Storage(_)) | Err(Error::Locked(_))
                ),
                "{failed}"
            );
        } else {
            let fresh = parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap();
            assert!(
                !wal.join("old").exists(),
                "{failed}: old WAL attached to fresh root"
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
        assert_eq!(std::fs::read(old_wal).unwrap(), b"old WAL", "{failed}");
    }
}

#[test]
#[cfg(unix)]
fn reset_durable_wal_barrier_precedes_root_move_and_deletion() {
    use std::cell::RefCell;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let wal = dir.path().join("external");
    drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap());
    let order = RefCell::new(Vec::new());
    reset_native_with_barrier(
        &root,
        Some(&wal),
        |step| {
            order.borrow_mut().push(format!("staged-{step}"));
            Ok(())
        },
        |stage, _| {
            order.borrow_mut().push(stage.to_string());
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        *order.borrow(),
        [
            "marker-dir",
            "wal-reserve-parent",
            "wal-old-parent",
            "wal-tombstone",
            "staged-wal",
            "root-reserve-parent",
            "root-old-parent",
            "root-tombstone",
            "staged-root"
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
    let (_, owner) = NativeVfs::lock_directory(&child).unwrap();
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
        NativeVfs::lock_directory(&child),
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
    let (_, owner) = NativeVfs::lock_directory(&child).unwrap();
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
    assert!(!std::fs::read_dir(dir.path()).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".reset-")));
    assert!(matches!(
        NativeVfs::lock_directory(&child),
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
    assert!(!std::fs::read_dir(dir.path()).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".reset-")));
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
    let (_, owner) = NativeVfs::lock_directory(&child).unwrap();
    std::fs::write(child.join("important"), b"independent wal-named database").unwrap();
    assert!(!root.join(LOCK_MARKER).exists());
    let err = reset_uri(root.to_str().unwrap(), None).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert!(!root.join(LOCK_MARKER).exists());
    assert_eq!(
        std::fs::read(child.join("important")).unwrap(),
        b"independent wal-named database"
    );
    assert!(!std::fs::read_dir(dir.path()).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".reset-")));
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
    assert!(!std::fs::read_dir(dir.path()).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".reset-")));
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
    assert!(!std::fs::read_dir(dir.path()).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".reset-")));
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
    assert!(!std::fs::read_dir(&parent).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".reset-")));
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
    for wal in [root.join("."), root.join("nested"), root.join("wal/nested")] {
        let error = reset_uri(root.to_str().unwrap(), Some(&wal)).unwrap_err();
        assert!(matches!(error, Error::Storage(_)), "{error}");
        assert!(root.join(LOCK_MARKER).exists());
        assert!(!wal.join(LOCK_MARKER).exists() || wal == root.join("."));
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
    assert!(!std::fs::read_dir(&parent).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".reset-")));
    drop(parse_uri(root.to_str().unwrap()).unwrap());
}

#[test]
fn native_writable_files_retain_both_leases_after_vfs_and_target_drop() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let wal = dir.path().join("external-wal");
    let other = dir.path().join("other");
    let target = parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap();
    let root_vfs = target.vfs();
    let (wal_vfs, _) = target.wal_vfs();
    let mut root_file = root_vfs.open("live", VfsOpen::create_new()).unwrap();
    let exclusive_file = root_vfs.open_exclusive("exclusive").unwrap().unwrap();
    let mut wal_file = wal_vfs.open("live", VfsOpen::create_new()).unwrap();
    drop((target, root_vfs, wal_vfs));
    assert!(matches!(
        parse_uri(root.to_str().unwrap()),
        Err(Error::Locked(_))
    ));
    assert!(matches!(
        parse_uri_with_wal(other.to_str().unwrap(), Some(wal.clone())),
        Err(Error::Locked(_))
    ));
    root_file.write_all(b"safe").unwrap();
    wal_file.write_all(b"safe").unwrap();
    drop(root_file);
    assert!(matches!(
        parse_uri(root.to_str().unwrap()),
        Err(Error::Locked(_))
    ));
    drop(wal_file);
    assert!(matches!(
        parse_uri(root.to_str().unwrap()),
        Err(Error::Locked(_))
    ));
    drop(exclusive_file);
    drop(parse_uri(root.to_str().unwrap()).unwrap());
    drop(parse_uri_with_wal(other.to_str().unwrap(), Some(wal)).unwrap());
}

#[test]
fn reset_handoff_keeps_fresh_root_and_wal_while_removing_only_old_tree() {
    use std::sync::mpsc;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let uri = root.to_str().unwrap();
    let old = parse_uri(uri).unwrap();
    let old_vfs = old.vfs();
    let mut old_file = old_vfs.open("wal/old.log", VfsOpen::create_new()).unwrap();
    old_file.write_all(b"old").unwrap();
    drop((old_file, old_vfs, old));

    let (ready_tx, ready_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let reset_path = root.clone();
    let reset = std::thread::spawn(move || {
        reset_native_with(&reset_path, None, |step| {
            ready_tx.send(step.to_string()).unwrap();
            resume_rx.recv().unwrap();
            Ok(())
        })
    });
    assert_eq!(ready_rx.recv().unwrap(), "root");
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
    use std::sync::mpsc;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let wal = dir.path().join("external-wal");
    let other = dir.path().join("other");
    drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap());
    let (ready_tx, ready_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let reset_root = root.clone();
    let reset_wal = wal.clone();
    let reset = std::thread::spawn(move || {
        reset_native_with(&reset_root, Some(&reset_wal), |step| {
            ready_tx.send(step.to_string()).unwrap();
            resume_rx.recv().unwrap();
            Ok(())
        })
    });
    assert_eq!(ready_rx.recv().unwrap(), "wal");
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
    assert_eq!(ready_rx.recv().unwrap(), "root");
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

#[test]
fn reset_child() {
    let Ok(mode) = std::env::var("CYPHERLITE_RESET_CHILD") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("CYPHERLITE_RESET_ROOT").unwrap());
    let wal = PathBuf::from(std::env::var_os("CYPHERLITE_RESET_WAL").unwrap());
    match mode.as_str() {
        #[cfg(unix)]
        "crash-wal" | "crash-root" => {
            let phase = mode.strip_prefix("crash-").unwrap();
            let _ = reset_native_with(&root, Some(&wal), |step| {
                if step == phase {
                    // Simulate abrupt termination, including no Drop of
                    // either lock or the staged tombstones.
                    std::process::exit(0);
                }
                Ok(())
            });
            panic!("reset did not reach requested crash phase {phase}");
        }
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
            .args([
                "--exact",
                "native_ownership::tests::reset_child",
                "--nocapture",
            ])
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
    use std::sync::mpsc;
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
    let (ready_tx, ready_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let reset_root = root.clone();
    let reset_wal = wal.clone();
    let reset = std::thread::spawn(move || {
        reset_native_with(&reset_root, Some(&reset_wal), |step| {
            ready_tx.send(step.to_string()).unwrap();
            resume_rx.recv().unwrap();
            Ok(())
        })
    });
    let run_child = |mode| {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_ownership::tests::reset_child",
                "--nocapture",
            ])
            .env("CYPHERLITE_RESET_CHILD", mode)
            .env("CYPHERLITE_RESET_ROOT", &root)
            .env("CYPHERLITE_RESET_WAL", &wal)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    assert_eq!(ready_rx.recv().unwrap(), "wal");
    run_child("locked");
    resume_tx.send(()).unwrap();
    assert_eq!(ready_rx.recv().unwrap(), "root");
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
        if step == "root" {
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

#[test]
#[cfg(unix)]
fn failed_reset_refuses_dangling_symlink_rollback_and_blocks_old_root() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let wal = dir.path().join("wal");
    let target = parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap();
    target
        .wal_vfs()
        .0
        .open("old", VfsOpen::create_new())
        .unwrap()
        .write_all(b"old")
        .unwrap();
    drop(target);
    let error = reset_native_with(&root, Some(&wal), |step| {
        assert_eq!(step, "wal");
        symlink(dir.path().join("missing"), &wal).unwrap();
        Err(Error::io("injected rollback"))
    })
    .unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert!(std::fs::symlink_metadata(&wal)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(matches!(
        parse_uri(root.to_str().unwrap()),
        Err(Error::Storage(_))
    ));
    let old_wal = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".wal.reset-")
        })
        .unwrap()
        .join("data");
    assert_eq!(std::fs::read(old_wal.join("old")).unwrap(), b"old");
    std::fs::remove_file(&wal).unwrap();
    std::fs::rename(&old_wal, &wal).unwrap();
    std::fs::remove_file(root.join(RESET_INCOMPLETE)).unwrap();
    drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal)).unwrap());
}

#[test]
#[cfg(unix)]
fn reset_default_wal_symlink_alias_removes_external_wal_and_alias() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let wal = dir.path().join("external-wal");
    drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap());
    std::fs::remove_dir_all(root.join("wal")).unwrap_or(());
    symlink(&wal, root.join("wal")).unwrap();
    let target = parse_uri(root.to_str().unwrap()).unwrap();
    target
        .vfs()
        .open("wal/old", VfsOpen::create_new())
        .unwrap()
        .write_all(b"old")
        .unwrap();
    drop(target);
    reset_uri(root.to_str().unwrap(), None).unwrap();
    assert!(!root.exists());
    assert!(!wal.exists());
}

#[test]
#[cfg(unix)]
fn default_wal_alias_retarget_does_not_redirect_open_replay_or_rotation() {
    use cypherlite_executor::RedoOp;
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let wal_a = dir.path().join("wal-a");
    let wal_b = dir.path().join("wal-b");
    let alias = root.join("wal");
    // Establish root and external WAL ownership before adding the supported
    // default alias. B is not leased and has a corrupt decoy:
    // opening/listing it would either fail replay or mutate these bytes.
    drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal_a.clone())).unwrap());
    std::fs::create_dir(&wal_b).unwrap();
    std::fs::write(wal_b.join("00000001.log"), b"unleased decoy").unwrap();
    symlink(&wal_a, &alias).unwrap();

    let target = parse_uri(root.to_str().unwrap()).unwrap();
    assert_eq!(
        cypherlite_core::file_identity::of_path(crate::wal_dir(&target).unwrap()).unwrap(),
        cypherlite_core::file_identity::of_path(&wal_a).unwrap()
    );
    assert_eq!(target.wal_vfs().1, "");
    std::fs::remove_file(&alias).unwrap();
    symlink(&wal_b, &alias).unwrap(); // between lease and open_store

    let (graph, mut wal) = open_store(&target).unwrap();
    assert_eq!(graph.live_node_count(), 0);
    wal.append_commit(
        1,
        &[RedoOp::CreateNode {
            id: 0,
            labels: vec!["Person".into()].into(),
            properties: Default::default(),
        }],
    )
    .unwrap();
    // Retarget again with an active WAL handle; rotate and read the closed
    // segment without following root/wal on either operation.
    std::fs::remove_file(&alias).unwrap();
    symlink(&wal_a, &alias).unwrap();
    std::fs::remove_file(&alias).unwrap();
    symlink(&wal_b, &alias).unwrap();
    assert_eq!(wal.rotate().unwrap(), 1);
    assert_eq!(wal.records_after(0, 10).unwrap().len(), 1);
    assert!(wal_a.join("00000001.log").exists());
    assert!(wal_a.join("00000002.log").exists());
    assert_eq!(
        std::fs::read(wal_b.join("00000001.log")).unwrap(),
        b"unleased decoy"
    );
    assert_eq!(std::fs::read_dir(&wal_b).unwrap().count(), 1);

    drop((wal, target));
    std::fs::remove_file(&alias).unwrap();
    symlink(&wal_a, &alias).unwrap();
    let reopened = parse_uri(root.to_str().unwrap()).unwrap();
    let (graph, wal) = open_store(&reopened).unwrap();
    assert_eq!(wal.lsn, 1);
    assert_eq!(graph.live_node_count(), 1);
    assert!(graph.node(0).is_some());
    assert_eq!(
        std::fs::read(wal_b.join("00000001.log")).unwrap(),
        b"unleased decoy"
    );
    assert_eq!(std::fs::read_dir(&wal_b).unwrap().count(), 1);
}

#[test]
fn rollback_noreplace_refuses_an_occupied_destination() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("from");
    let to = dir.path().join("to");
    std::fs::create_dir(&from).unwrap();
    std::fs::create_dir(&to).unwrap();
    assert!(rename_reset_noreplace(&from, &to).is_err());
    assert!(from.exists());
    assert!(to.exists());
}

#[test]
#[cfg(unix)]
fn failed_reset_staging_restores_intact_root_and_external_wal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let wal = dir.path().join("external-wal");
    let target = parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap();
    let (graph, memory_wal) = person_ada();
    checkpoint_with_lsn(&target, &graph, memory_wal.lsn, None).unwrap();
    let root_vfs = target.vfs();
    let (wal_vfs, _) = target.wal_vfs();
    root_vfs
        .open("old", VfsOpen::create_new())
        .unwrap()
        .write_all(b"root")
        .unwrap();
    wal_vfs
        .open("old", VfsOpen::create_new())
        .unwrap()
        .write_all(b"wal")
        .unwrap();
    drop((target, root_vfs, wal_vfs));
    let error = reset_native_with(&root, Some(&wal), |step| {
        if step == "wal" {
            Err(Error::io("injected"))
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert!(matches!(error, Error::Io(_)), "{error}");
    assert_eq!(std::fs::read(root.join("old")).unwrap(), b"root");
    assert_eq!(std::fs::read(wal.join("old")).unwrap(), b"wal");
    let recovered = parse_uri_with_wal(root.to_str().unwrap(), Some(wal)).unwrap();
    assert_eq!(load(&recovered).unwrap().live_node_count(), 1);
}

#[test]
fn failed_reset_does_not_replace_fresh_owner_and_retains_recoverable_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let old = parse_uri(root.to_str().unwrap()).unwrap();
    let (graph, memory_wal) = person_ada();
    checkpoint_with_lsn(&old, &graph, memory_wal.lsn, None).unwrap();
    drop(old);
    let error = reset_native_with(&root, None, |_| {
        let fresh = parse_uri(root.to_str().unwrap())?;
        let mut file = fresh.vfs().open("new", VfsOpen::create_new())?;
        file.write_all(b"fresh")?;
        Err(Error::io("injected after fresh open"))
    })
    .unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert_eq!(std::fs::read(root.join("new")).unwrap(), b"fresh");
    let container = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".db.reset-")
        })
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&container).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    let tombstone = container.join("data");
    // The tombstone path is canonicalized by the reset planner; canonicalize
    // the test's own spelling so the Windows short-path/`\\?\` prefix does not
    // defeat the substring check.
    let tombstone_canonical = std::fs::canonicalize(&tombstone).unwrap();
    assert!(error
        .to_string()
        .contains(&tombstone_canonical.display().to_string()));
    let recovered = parse_uri(tombstone.to_str().unwrap()).unwrap();
    assert_eq!(load(&recovered).unwrap().live_node_count(), 1);
    drop(recovered);
    reset_uri(root.to_str().unwrap(), None).unwrap();
    std::fs::rename(&tombstone, &root).unwrap();
    let restored = parse_uri(root.to_str().unwrap()).unwrap();
    assert_eq!(load(&restored).unwrap().live_node_count(), 1);
}

#[cfg(unix)]
mod native_tests {
    use super::*;
    #[test]
    fn raced_in_untyped_wal_is_not_claimed_or_removed() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("manifest.json"), b"legacy root evidence").unwrap();
        for existing_lock in [false, true] {
            let wal = dir.path().join(format!("disjoint-wal-{existing_lock}"));
            let mut marker_inode = None;
            let error = lease_with(&root, &wal, || {
                // Between both preflights and final-component creation.
                fs::create_dir(&wal).unwrap();
                if existing_lock {
                    fs::write(wal.join(LOCK_MARKER), b"").unwrap();
                    marker_inode = Some(fs::metadata(wal.join(LOCK_MARKER)).unwrap().ino());
                }
            })
            .err()
            .unwrap();
            assert!(matches!(error, Error::Storage(_)), "{error}");
            assert!(wal.is_dir());
            if let Some(inode) = marker_inode {
                assert_eq!(fs::read(wal.join(LOCK_MARKER)).unwrap(), b"");
                assert_eq!(fs::metadata(wal.join(LOCK_MARKER)).unwrap().ino(), inode);
            } else {
                assert!(!wal.join(LOCK_MARKER).exists());
            }
        }
        assert_eq!(
            fs::read(root.join("manifest.json")).unwrap(),
            b"legacy root evidence"
        );
    }

    #[test]
    fn preflight_waits_for_existing_inode_lease_before_reading_role() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        let marker = root.join(LOCK_MARKER);
        fs::write(&marker, DirectoryRole::Root.marker()).unwrap();
        let owner = NativeVfs::lock_regular_file(&marker).unwrap();
        assert!(matches!(
            preflight_role(
                &root,
                DirectoryRole::Root,
                RoleCheck {
                    reset: true,
                    direct_wal: false,
                    newly_created: false,
                }
            ),
            Err(Error::Locked(_))
        ));
        drop(owner);
        assert!(preflight_role(
            &root,
            DirectoryRole::Root,
            RoleCheck {
                reset: true,
                direct_wal: false,
                newly_created: false,
            }
        )
        .unwrap());
        assert_eq!(fs::read(marker).unwrap(), DirectoryRole::Root.marker());
    }

    #[test]
    fn wal_preflight_contention_does_not_create_root_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("legacy-root");
        let wal = dir.path().join("external-wal");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("manifest.json"), b"legacy root evidence").unwrap();
        fs::create_dir(&wal).unwrap();
        let marker = wal.join(LOCK_MARKER);
        fs::write(&marker, DirectoryRole::Wal.marker()).unwrap();
        let owner = NativeVfs::lock_regular_file(&marker).unwrap();
        let error = lease(&root, &wal).err().unwrap();
        assert!(matches!(error, Error::Locked(_)), "{error}");
        assert!(!root.join(LOCK_MARKER).exists());
        assert_eq!(fs::read(&marker).unwrap(), DirectoryRole::Wal.marker());
        drop(owner);
        drop(lease(&root, &wal).unwrap());
    }
}
