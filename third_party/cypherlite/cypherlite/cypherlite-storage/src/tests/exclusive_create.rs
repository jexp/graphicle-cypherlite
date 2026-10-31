//! ADR-0161: exclusive destination creation (`parse_uri_creating`) for the
//! clone/snapshot/restore publish path. The loser of a create race is refused
//! with destination-already-exists and the pre-existing destination survives
//! byte-identical; ordinary opens keep adopting existing stores.

use super::*;

/// The ADR-0161 refusal: `result` must fail with a destination-already-exists
/// storage error. A site needing a looser or narrower assertion matches on
/// the error itself instead (see the dangling-symlink site below).
fn expect_destination_exists<T>(result: Result<T>) {
    let err = result.err().expect("exclusive create must refuse");
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.starts_with(DESTINATION_EXISTS)),
        "{err}"
    );
}

#[test]
fn parse_uri_creating_creates_a_missing_destination() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("fresh.cl");
    drop(parse_uri_creating(bundle.to_str().unwrap(), None, None).unwrap());
    assert!(bundle.is_file());

    let root = dir.path().join("fresh");
    drop(parse_uri_creating(root.to_str().unwrap(), None, None).unwrap());
    assert!(root.join(LOCK_MARKER).is_file());
    assert!(root.join("wal").join(LOCK_MARKER).is_file());
}

#[test]
fn parse_uri_creating_refuses_an_existing_bundle_without_touching_it() {
    let dir = tempfile::tempdir().unwrap();

    // Arbitrary pre-existing bytes at the destination: refused, not opened.
    let victim = dir.path().join("victim.cl");
    std::fs::write(&victim, b"pre-existing victim bytes").unwrap();
    expect_destination_exists(parse_uri_creating(victim.to_str().unwrap(), None, None));
    assert_eq!(
        std::fs::read(&victim).unwrap(),
        b"pre-existing victim bytes"
    );

    // A valid bundle held open by its winner: the loser is refused and the
    // bundle is untouched; ordinary open adopts it once the winner closes.
    let path = dir.path().join("graph.cl");
    let winner = BundleVfs::open_create_exclusive(&path).unwrap();
    expect_destination_exists(parse_uri_creating(path.to_str().unwrap(), None, None));
    expect_destination_exists(BundleVfs::open_create_exclusive(&path));
    drop(winner);
    let before = std::fs::read(&path).unwrap();
    drop(BundleVfs::open_or_create(&path).unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn parse_uri_creating_refuses_an_existing_directory_without_touching_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("dest");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("sentinel"), b"victim").unwrap();

    expect_destination_exists(parse_uri_creating(root.to_str().unwrap(), None, None));
    assert_eq!(std::fs::read(root.join("sentinel")).unwrap(), b"victim");
    assert!(
        !root.join(LOCK_MARKER).exists(),
        "refused create must not plant a marker in the pre-existing directory"
    );

    // The loser of a create race against a live winner is likewise refused,
    // and the winner keeps its exclusive writer lease throughout.
    let live = dir.path().join("live");
    let winner = parse_uri_creating(live.to_str().unwrap(), None, None).unwrap();
    expect_destination_exists(parse_uri_creating(live.to_str().unwrap(), None, None));
    assert!(matches!(
        parse_uri(live.to_str().unwrap()),
        Err(Error::Locked(_))
    ));
    drop(winner);
    // Ordinary open still adopts the directory once the winner closes.
    drop(parse_uri(live.to_str().unwrap()).unwrap());
}

#[test]
fn parse_uri_creating_refuses_a_directory_at_the_bundle_path() {
    let dir = tempfile::tempdir().unwrap();
    // A directory raced into the check-create window at the `.cl` bundle
    // path: `create_new` fails (`IsADirectory` on Unix, `AccessDenied` on
    // Windows), and the post-failure `symlink_metadata` check maps either to
    // the same destination-already-exists refusal as `AlreadyExists` — never
    // a generic I/O error, so publish cleanup cannot `remove_dir_all` a
    // directory this run never created (ADR-0161).
    let root = dir.path().join("dest.cl");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("sentinel"), b"victim").unwrap();

    expect_destination_exists(parse_uri_creating(root.to_str().unwrap(), None, None));
    expect_destination_exists(BundleVfs::open_create_exclusive(&root));
    assert_eq!(std::fs::read(root.join("sentinel")).unwrap(), b"victim");
}

#[cfg(unix)]
#[test]
fn parse_uri_creating_never_follows_a_symlinked_destination() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();

    // `.cl` destination: a symlink raced into the window is refused, never
    // followed; the victim behind it keeps its bytes and the link survives.
    let victim = dir.path().join("victim.bin");
    std::fs::write(&victim, b"victim bytes").unwrap();
    let alias = dir.path().join("link.cl");
    symlink(&victim, &alias).unwrap();
    expect_destination_exists(parse_uri_creating(alias.to_str().unwrap(), None, None));
    assert_eq!(std::fs::read(&victim).unwrap(), b"victim bytes");
    assert_eq!(std::fs::read_link(&alias).unwrap(), victim);

    // A dangling symlink is an existing directory entry too: refused (the
    // nested-identity check cannot canonicalize it), the link survives, and
    // nothing is created at its target. Which guard fires first is not the
    // property under test, so this site keeps its looser Storage|Io match
    // instead of `expect_destination_exists`.
    let dangling = dir.path().join("dangling.cl");
    symlink(dir.path().join("missing"), &dangling).unwrap();
    let err = parse_uri_creating(dangling.to_str().unwrap(), None, None)
        .err()
        .expect("exclusive create must refuse");
    assert!(matches!(err, Error::Storage(_) | Error::Io(_)), "{err}");
    assert!(std::fs::symlink_metadata(&dangling).is_ok());
    assert!(!dir.path().join("missing").exists());

    // Native directory destination: a symlinked final component is refused,
    // never followed, and the victim directory gains no marker.
    let victim_dir = dir.path().join("victim-dir");
    std::fs::create_dir(&victim_dir).unwrap();
    std::fs::write(victim_dir.join("keep"), b"keep").unwrap();
    let dir_alias = dir.path().join("dir-link");
    symlink(&victim_dir, &dir_alias).unwrap();
    expect_destination_exists(parse_uri_creating(dir_alias.to_str().unwrap(), None, None));
    assert_eq!(std::fs::read(victim_dir.join("keep")).unwrap(), b"keep");
    assert!(!victim_dir.join(LOCK_MARKER).exists());
    assert_eq!(std::fs::read_link(&dir_alias).unwrap(), victim_dir);
}

#[cfg(windows)]
#[test]
fn parse_uri_creating_never_follows_a_symlinked_destination() {
    let dir = tempfile::tempdir().unwrap();
    let victim = dir.path().join("victim.bin");
    std::fs::write(&victim, b"victim bytes").unwrap();
    let alias = dir.path().join("link.cl");
    if let Err(err) = std::os::windows::fs::symlink_file(&victim, &alias) {
        // Some Windows runners do not grant the symlink privilege.
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        return;
    }
    expect_destination_exists(parse_uri_creating(alias.to_str().unwrap(), None, None));
    assert_eq!(std::fs::read(&victim).unwrap(), b"victim bytes");
    assert!(std::fs::symlink_metadata(&alias).is_ok());
}
