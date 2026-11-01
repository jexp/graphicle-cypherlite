//! VFS tests: shared atomic-write/locking semantics over every
//! backend, plus per-backend behaviors. Extracted from `mod.rs`
//! (the no-1k-line rule, LABS-311).

use super::*;
use std::sync::{Arc, Mutex};

#[test]
fn capped_vfs_writer_does_not_write_past_its_limit() {
    let vfs = MemoryVfs::new();
    let mut file = vfs.open("candidate", VfsOpen::create_new()).unwrap();
    {
        let mut writer = CappedVfsWriter::new(&mut *file, 3, "candidate is too large");
        assert_eq!(writer.write(b"abc").unwrap(), 3);
        assert!(writer.write(b"d").is_err());
        assert!(writer.exceeded());
    }
    assert_eq!(file.len().unwrap(), 3);
}

// These wrappers deliberately claim durable storage while omitting one or
// both directory-persistence methods. The underlying bytes remain usable
// so an atomic write can reach the post-rename error path.
struct MissingDirectoryBarriers {
    inner: MemoryVfs,
}

impl Vfs for MissingDirectoryBarriers {
    fn durable(&self) -> bool {
        true
    }
    fn create_dir_all(&self, path: &str) -> Result<()> {
        self.inner.create_dir_all(path)
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>> {
        self.inner.list_dir(path)
    }
    fn exists(&self, path: &str) -> Result<bool> {
        self.inner.exists(path)
    }
    fn metadata(&self, path: &str) -> Result<VfsMeta> {
        self.inner.metadata(path)
    }
    fn open(&self, path: &str, opts: VfsOpen) -> Result<Box<dyn VfsFile>> {
        self.inner.open(path, opts)
    }
    fn remove_file(&self, path: &str) -> Result<()> {
        self.inner.remove_file(path)
    }
    fn remove_dir_all(&self, path: &str) -> Result<()> {
        self.inner.remove_dir_all(path)
    }
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.inner.rename(from, to)
    }
}

struct MissingSync(MissingDirectoryBarriers);

impl Vfs for MissingSync {
    fn durable(&self) -> bool {
        self.0.durable()
    }
    fn create_dir_all(&self, path: &str) -> Result<()> {
        self.0.create_dir_all(path)
    }
    fn create_dir_all_for_atomic(&self, path: &str) -> Result<()> {
        self.0.create_dir_all(path)
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>> {
        self.0.list_dir(path)
    }
    fn exists(&self, path: &str) -> Result<bool> {
        self.0.exists(path)
    }
    fn metadata(&self, path: &str) -> Result<VfsMeta> {
        self.0.metadata(path)
    }
    fn open(&self, path: &str, opts: VfsOpen) -> Result<Box<dyn VfsFile>> {
        self.0.open(path, opts)
    }
    fn remove_file(&self, path: &str) -> Result<()> {
        self.0.remove_file(path)
    }
    fn remove_dir_all(&self, path: &str) -> Result<()> {
        self.0.remove_dir_all(path)
    }
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.0.rename(from, to)
    }
}

#[test]
fn durable_vfs_missing_directory_barriers_cannot_claim_atomic_success() {
    let vfs = MissingDirectoryBarriers {
        inner: MemoryVfs::new(),
    };
    let error = atomic_write(&vfs, "nested/value", b"new").unwrap_err();
    assert!(error.to_string().contains("create_dir_all_for_atomic"));
    assert!(!vfs.exists("nested/value").unwrap());

    let vfs = MissingSync(vfs);
    let error = atomic_write(&vfs, "nested/value", b"new").unwrap_err();
    assert!(error.to_string().contains("sync_dir"));
    assert_eq!(read_to_vec(&vfs, "nested/value").unwrap(), b"new");
    assert_eq!(vfs.list_dir("nested").unwrap(), ["value"]);
}

type CollisionHook<'a> = Box<dyn Fn(&str) + Send + Sync + 'a>;

/// The closed vocabulary of atomic-write stages the fault harness traces
/// and can fail — one typed stage kind instead of string literals matched
/// at every site, and a collection of them so a scenario can fail several
/// stages at once (rename AND cleanup).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FaultStage {
    Parents,
    ExclusiveOpen,
    Collision,
    Write,
    FileSync,
    Close,
    Rename,
    DirSync,
    Cleanup,
    PostRenameError,
}

impl FaultStage {
    const fn name(self) -> &'static str {
        match self {
            Self::Parents => "parents",
            Self::ExclusiveOpen => "exclusive_open",
            Self::Collision => "collision",
            Self::Write => "write",
            Self::FileSync => "file_sync",
            Self::Close => "close",
            Self::Rename => "rename",
            Self::DirSync => "dir_sync",
            Self::Cleanup => "cleanup",
            Self::PostRenameError => "post_rename_error",
        }
    }
}

type FaultStages = Arc<Vec<FaultStage>>;

struct FaultVfs<'a> {
    inner: &'a dyn Vfs,
    events: Arc<Mutex<Vec<FaultStage>>>,
    fail: FaultStages,
    collision: Option<CollisionHook<'a>>,
    collided: Mutex<Option<String>>,
}

impl<'a> FaultVfs<'a> {
    fn new(inner: &'a dyn Vfs, fail: &[FaultStage]) -> Self {
        Self {
            inner,
            events: Arc::new(Mutex::new(Vec::new())),
            fail: Arc::new(fail.to_vec()),
            collision: None,
            collided: Mutex::new(None),
        }
    }

    fn event(&self, stage: FaultStage) -> Result<()> {
        self.events.lock().unwrap().push(stage);
        if self.fail.contains(&stage) {
            Err(Error::io(format!("injected {} failure", stage.name())))
        } else {
            Ok(())
        }
    }

    fn trace(&self) -> Vec<FaultStage> {
        self.events.lock().unwrap().clone()
    }
}

struct FaultFile {
    inner: Option<Box<dyn VfsFile>>,
    events: Arc<Mutex<Vec<FaultStage>>>,
    fail: FaultStages,
}

impl Drop for FaultFile {
    fn drop(&mut self) {
        drop(self.inner.take());
        self.events.lock().unwrap().push(FaultStage::Close);
    }
}

impl VfsFile for FaultFile {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.events.lock().unwrap().push(FaultStage::Write);
        if self.fail.contains(&FaultStage::Write) {
            return Err(Error::io("injected write failure"));
        }
        self.inner.as_mut().unwrap().write_all(buf)
    }
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.inner.as_mut().unwrap().read_exact(buf)
    }
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        self.inner.as_mut().unwrap().seek(pos)
    }
    fn sync_all(&mut self) -> Result<()> {
        self.events.lock().unwrap().push(FaultStage::FileSync);
        if self.fail.contains(&FaultStage::FileSync) {
            return Err(Error::io("injected file_sync failure"));
        }
        self.inner.as_mut().unwrap().sync_all()
    }
    fn len(&self) -> Result<u64> {
        self.inner.as_ref().unwrap().len()
    }
    fn set_len(&mut self, len: u64) -> Result<()> {
        self.inner.as_mut().unwrap().set_len(len)
    }
}

impl Vfs for FaultVfs<'_> {
    fn durable(&self) -> bool {
        self.inner.durable()
    }
    fn create_dir_all(&self, path: &str) -> Result<()> {
        self.inner.create_dir_all(path)
    }
    fn create_dir_all_for_atomic(&self, path: &str) -> Result<()> {
        self.event(FaultStage::Parents)?;
        self.inner.create_dir_all_for_atomic(path)
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>> {
        self.inner.list_dir(path)
    }
    fn exists(&self, path: &str) -> Result<bool> {
        self.inner.exists(path)
    }
    fn metadata(&self, path: &str) -> Result<VfsMeta> {
        self.inner.metadata(path)
    }
    fn open(&self, path: &str, opts: VfsOpen) -> Result<Box<dyn VfsFile>> {
        self.inner.open(path, opts)
    }
    fn open_exclusive(&self, path: &str) -> Result<Option<Box<dyn VfsFile>>> {
        self.event(FaultStage::ExclusiveOpen)?;
        if self.fail.contains(&FaultStage::Collision) {
            return Ok(None);
        }
        if let Some(collision) = &self.collision {
            let mut collided = self.collided.lock().unwrap();
            if collided.is_none() {
                collision(path);
                *collided = Some(path.to_string());
            }
        }
        self.inner.open_exclusive(path).map(|f| {
            f.map(|inner| {
                Box::new(FaultFile {
                    inner: Some(inner),
                    events: Arc::clone(&self.events),
                    fail: Arc::clone(&self.fail),
                }) as Box<dyn VfsFile>
            })
        })
    }
    fn remove_file(&self, path: &str) -> Result<()> {
        self.event(FaultStage::Cleanup)?;
        self.inner.remove_file(path)
    }
    fn remove_dir_all(&self, path: &str) -> Result<()> {
        self.inner.remove_dir_all(path)
    }
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.event(FaultStage::Rename)?;
        self.inner.rename(from, to)?;
        if self.fail.contains(&FaultStage::PostRenameError) {
            return Err(Error::io("injected post_rename_error failure"));
        }
        Ok(())
    }
    fn sync_dir(&self, path: &str) -> Result<()> {
        self.event(FaultStage::DirSync)?;
        self.inner.sync_dir(path)
    }
}

fn atomic_faults(vfs: &dyn Vfs) {
    atomic_write(vfs, "nested/object", b"old").unwrap();
    for stage in [FaultStage::Parents, FaultStage::ExclusiveOpen] {
        let fault = FaultVfs::new(vfs, &[stage]);
        assert!(atomic_write(&fault, "nested/object", b"new")
            .unwrap_err()
            .to_string()
            .contains(stage.name()));
        assert_eq!(read_to_vec(vfs, "nested/object").unwrap(), b"old");
        assert_eq!(vfs.list_dir("nested").unwrap(), ["object"]);
    }
    for stage in [
        FaultStage::Write,
        FaultStage::FileSync,
        FaultStage::Rename,
        FaultStage::DirSync,
    ] {
        let fault = FaultVfs::new(vfs, &[stage]);
        let error = atomic_write(&fault, "nested/object", b"new").unwrap_err();
        assert!(error.to_string().contains(stage.name()), "{error}");
        assert_eq!(
            read_to_vec(vfs, "nested/object").unwrap(),
            if stage == FaultStage::DirSync {
                b"new"
            } else {
                b"old"
            }
        );
        assert_eq!(vfs.list_dir("nested").unwrap(), ["object"]);
        let trace = fault.trace();
        assert!(trace.starts_with(&[
            FaultStage::Parents,
            FaultStage::ExclusiveOpen,
            FaultStage::Write
        ]));
        assert!(trace.contains(&FaultStage::Close));
        if stage == FaultStage::DirSync {
            assert_eq!(
                trace,
                [
                    FaultStage::Parents,
                    FaultStage::ExclusiveOpen,
                    FaultStage::Write,
                    FaultStage::FileSync,
                    FaultStage::Close,
                    FaultStage::Rename,
                    FaultStage::DirSync
                ]
            );
        } else {
            assert_eq!(trace.last(), Some(&FaultStage::Cleanup));
            atomic_write(vfs, "nested/object", b"old").unwrap();
        }
    }
    // Failed cleanup retains the original error and reports its own
    // failure — the one scenario needing two simultaneous failures.
    let fault = FaultVfs::new(vfs, &[FaultStage::Rename, FaultStage::Cleanup]);
    let error = atomic_write(&fault, "nested/object", b"new").unwrap_err();
    assert!(error.to_string().contains("injected rename failure"));
    assert!(error.to_string().contains("temporary file cleanup failed"));
    assert_eq!(read_to_vec(vfs, "nested/object").unwrap(), b"new");
    let temp = vfs
        .list_dir("nested")
        .unwrap()
        .into_iter()
        .find(|name| name.starts_with("object.tmp-"))
        .unwrap();
    vfs.remove_file(&format!("nested/{temp}")).unwrap();

    // Some backends can publish and then return a rename error. Cleanup
    // must never touch the new destination after the temp was consumed.
    let fault = FaultVfs::new(vfs, &[FaultStage::PostRenameError]);
    assert!(atomic_write(&fault, "nested/object", b"published")
        .unwrap_err()
        .to_string()
        .contains(FaultStage::PostRenameError.name()));
    assert_eq!(read_to_vec(vfs, "nested/object").unwrap(), b"published");
    assert_eq!(vfs.list_dir("nested").unwrap(), ["object"]);
    assert!(!fault.trace().contains(&FaultStage::Cleanup));
}

fn atomic_collision(vfs: &dyn Vfs) {
    let mut fault = FaultVfs::new(vfs, &[]);
    fault.collision = Some(Box::new(|path| {
        let mut f = vfs.open(path, VfsOpen::create_new()).unwrap();
        f.write_all(b"occupied").unwrap();
    }));
    atomic_write(&fault, "collisions/value", b"new").unwrap();
    let occupied = fault.collided.lock().unwrap().clone().unwrap();
    assert_eq!(read_to_vec(vfs, &occupied).unwrap(), b"occupied");
    assert_eq!(read_to_vec(vfs, "collisions/value").unwrap(), b"new");
    assert_eq!(
        fault
            .trace()
            .iter()
            .filter(|&&stage| stage == FaultStage::ExclusiveOpen)
            .count(),
        2
    );
    vfs.remove_file(&occupied).unwrap();

    let fault = FaultVfs::new(vfs, &[FaultStage::Collision]);
    let error = atomic_write(&fault, "collisions/value", b"other").unwrap_err();
    assert!(error.to_string().contains("after 8 collisions"));
    assert_eq!(read_to_vec(vfs, "collisions/value").unwrap(), b"new");
    assert_eq!(
        fault
            .trace()
            .iter()
            .filter(|&&stage| stage == FaultStage::ExclusiveOpen)
            .count(),
        TEMP_ATTEMPTS
    );
}

fn roundtrip(vfs: &dyn Vfs) {
    vfs.create_dir_all("wal").unwrap();
    atomic_write(vfs, "obj.bin", b"one").unwrap();
    assert_eq!(read_to_vec(vfs, "obj.bin").unwrap(), b"one");
    atomic_write(vfs, "obj.bin", b"two").unwrap();
    assert_eq!(read_to_vec(vfs, "obj.bin").unwrap(), b"two");
    assert!(!vfs.exists("obj.bin.tmp").unwrap());
    atomic_write(vfs, "nodes/part.parquet", b"shard").unwrap();
    assert!(vfs.metadata("nodes").unwrap().is_dir());
    let mut names = vfs.list_dir("wal").unwrap();
    names.sort();
    assert!(names.is_empty());
    {
        let mut f = vfs
            .open("wal/00000001.log", VfsOpen::create_truncate())
            .unwrap();
        f.write_all(crate::wal::MAGIC).unwrap();
        f.sync_all().unwrap();
    }
    let listed = vfs.list_dir("wal").unwrap();
    assert_eq!(listed, vec!["00000001.log".to_string()]);
    vfs.remove_file("wal/00000001.log").unwrap();
    vfs.remove_file("missing.log").unwrap();
    vfs.remove_dir_all("nodes").unwrap();
    assert!(!vfs.exists("nodes/part.parquet").unwrap());
}

#[test]
fn memory_vfs_roundtrip() {
    let vfs = MemoryVfs::new();
    assert!(!vfs.durable());
    roundtrip(&vfs);
    atomic_faults(&vfs);
    atomic_collision(&vfs);
}

#[test]
fn native_vfs_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let vfs = NativeVfs::new(dir.path());
    assert!(vfs.durable());
    roundtrip(&vfs);
    atomic_faults(&vfs);
    atomic_collision(&vfs);
    assert_eq!(std::fs::read(dir.path().join("obj.bin")).unwrap(), b"two");
}

#[test]
fn bundle_vfs_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.cl");
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    assert!(vfs.durable());
    roundtrip(&vfs);
    atomic_faults(&vfs);
    atomic_collision(&vfs);
    drop(vfs);
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    assert_eq!(read_to_vec(&vfs, "obj.bin").unwrap(), b"two");
    assert!(path.is_file());
    assert!(!dir.path().join("wal").exists());
    assert_eq!(dir.path().read_dir().unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn native_atomic_temp_symlink_collision_never_follows_target() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let dir = tempfile::tempdir().unwrap();
    let vfs = NativeVfs::new(dir.path());
    let target = dir.path().join("victim");
    std::fs::write(&target, b"secret").unwrap();
    symlink(&target, dir.path().join("value.tmp")).unwrap();
    // A dangling symlink occupying a randomly chosen name is also a collision.
    let mut fault = FaultVfs::new(&vfs, &[]);
    fault.collision = Some(Box::new(|path| {
        symlink(dir.path().join("missing-target"), dir.path().join(path)).unwrap();
    }));
    atomic_write(&fault, "value", b"new").unwrap();
    let collided = fault.collided.lock().unwrap().clone().unwrap();
    assert!(std::fs::symlink_metadata(dir.path().join(collided))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read(&target).unwrap(), b"secret");
    assert_eq!(std::fs::read(dir.path().join("value")).unwrap(), b"new");
    assert!(std::fs::symlink_metadata(dir.path().join("value.tmp"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::metadata(dir.path().join("value"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn native_atomic_creates_and_syncs_ancestor_directories() {
    let dir = tempfile::tempdir().unwrap();
    let vfs = NativeVfs::new(dir.path().join("database"));
    atomic_write(&vfs, "a/b/value", b"durable").unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("database/a/b/value")).unwrap(),
        b"durable"
    );
    assert!(vfs.sync_dir("a/b").is_ok());
    assert!(vfs.sync_dir("a/missing").is_err());
}

#[test]
fn bundle_tmp_sync_without_rename_does_not_publish() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.cl");
    {
        let vfs = BundleVfs::open_or_create(&path).unwrap();
        let mut f = vfs.open("obj.bin.tmp", VfsOpen::create_truncate()).unwrap();
        f.write_all(b"secret").unwrap();
        f.sync_all().unwrap();
    }
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    assert!(!vfs.exists("obj.bin").unwrap());
    assert_eq!(read_to_vec(&vfs, "obj.bin.tmp").unwrap(), b"secret");
}

#[test]
fn memory_vfs_is_shareable() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    atomic_write(&*vfs, "a", b"x").unwrap();
    assert_eq!(read_to_vec(&*vfs, "a").unwrap(), b"x");
}

#[test]
fn hex_16_is_lowercase_fixed_width_and_deterministic() {
    assert_eq!(hex_16([0xab; 16]), "ab".repeat(16));
    assert_eq!(hex_16([0x00; 16]), "00".repeat(16));
    assert_eq!(hex_16([0x0f; 16]), "0f".repeat(16));
}

#[test]
fn random_hex_16_is_unique_canonical_token() {
    let first = random_hex_16().unwrap();
    let second = random_hex_16().unwrap();
    assert!(is_lower_hex_32(&first));
    assert_ne!(first, second);
}

#[test]
fn is_lower_hex_32_rejects_noncanonical_tokens() {
    assert!(is_lower_hex_32("0123456789abcdef0123456789abcdef"));
    for bad in [
        "0123456789abcdef0123456789abcde",   // 31 chars
        "0123456789abcdef0123456789abcdef0", // 33 chars
        "0123456789abcdef0123456789ABCDEF",  // uppercase
        "0123456789abcdef0123456789abcdeg",  // non-hex
        "",
    ] {
        assert!(!is_lower_hex_32(bad), "{bad}");
    }
}
