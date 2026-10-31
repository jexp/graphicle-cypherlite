//! Publish destination create race (ADR-0161): a destination that appears
//! between the existence pre-check and the exclusive create is refused with
//! destination-already-exists and left byte-identical — the loser of the
//! race deletes nothing it did not create.

use std::fs;
use std::path::{Path, PathBuf};

use cypherlite::test_opts::{set_plant_destination, writable, PlantShape, PLANTED_SENTINEL};
use cypherlite::{CloneOptions, Database, Error};
use cypherlite_test_support::uri;

struct ClearPlant;

impl Drop for ClearPlant {
    fn drop(&mut self) {
        set_plant_destination(None);
    }
}

fn source(dir: &Path) -> PathBuf {
    let src = dir.join("src");
    let writer = Database::open_with(uri(&src), writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    src
}

#[test]
fn raced_in_bundle_destination_is_refused_and_left_untouched() {
    let _clear = ClearPlant;
    let dir = tempfile::tempdir().unwrap();
    let src = source(dir.path());
    let bundle = dir.path().join("backup.cl");

    // The destination appears in the check-create window (the finding's two
    // concurrent `clone x y.cl` runs): refused, never adopted, and the
    // planted stand-in for the winner's in-progress bundle is untouched.
    set_plant_destination(Some(PlantShape::MatchDestination));
    let err = Database::clone(uri(&src), uri(&bundle), CloneOptions::default()).unwrap_err();
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.contains("clone destination already exists")),
        "{err}"
    );
    assert_eq!(fs::read(&bundle).unwrap(), PLANTED_SENTINEL);

    // The hook fired exactly once: a clean retry publishes normally.
    Database::clone(
        uri(&src),
        uri(&dir.path().join("ok.cl")),
        CloneOptions::default(),
    )
    .unwrap();
}

#[test]
fn raced_in_directory_destination_is_refused_and_left_untouched() {
    let _clear = ClearPlant;
    let dir = tempfile::tempdir().unwrap();
    let src = source(dir.path());

    // dir source: native directory destination planted in the window.
    let dst = dir.path().join("dst");
    set_plant_destination(Some(PlantShape::MatchDestination));
    let err = Database::clone(uri(&src), uri(&dst), CloneOptions::default()).unwrap_err();
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.contains("clone destination already exists")),
        "{err}"
    );
    assert_eq!(fs::read(dst.join("sentinel")).unwrap(), PLANTED_SENTINEL);
    assert!(
        !dst.join("wal").exists(),
        "refused clone must not initialize the planted directory"
    );

    // bundle source: native directory destination planted in the window.
    let bundle = dir.path().join("backup.cl");
    Database::clone(uri(&src), uri(&bundle), CloneOptions::default()).unwrap();
    let restored = dir.path().join("restored");
    set_plant_destination(Some(PlantShape::MatchDestination));
    let err = Database::clone(uri(&bundle), uri(&restored), CloneOptions::default()).unwrap_err();
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.contains("clone destination already exists")),
        "{err}"
    );
    assert_eq!(
        fs::read(restored.join("sentinel")).unwrap(),
        PLANTED_SENTINEL
    );
    assert!(!restored.join("wal").exists());

    // The hook fired exactly once per publish: a clean retry clones.
    Database::clone(
        uri(&bundle),
        uri(&dir.path().join("ok")),
        CloneOptions::default(),
    )
    .unwrap();
}

#[test]
fn raced_in_directory_at_bundle_path_is_refused_and_left_untouched() {
    let _clear = ClearPlant;
    let dir = tempfile::tempdir().unwrap();
    let src = source(dir.path());
    let bundle = dir.path().join("backup.cl");

    // A directory raced into the check-create window at the `.cl` path:
    // `create_new` fails (`IsADirectory` on Unix, `AccessDenied` on
    // Windows), and the post-failure `symlink_metadata` check maps either to
    // the same refusal as `AlreadyExists` — never a generic I/O error, so
    // `remove_created` cannot `remove_dir_all` a directory this run never
    // created.
    set_plant_destination(Some(PlantShape::Directory));
    let err = Database::clone(uri(&src), uri(&bundle), CloneOptions::default()).unwrap_err();
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.contains("clone destination already exists")),
        "{err}"
    );
    assert_eq!(fs::read(bundle.join("sentinel")).unwrap(), PLANTED_SENTINEL);

    // The hook fired exactly once: a clean retry publishes normally.
    Database::clone(
        uri(&src),
        uri(&dir.path().join("ok.cl")),
        CloneOptions::default(),
    )
    .unwrap();
}
