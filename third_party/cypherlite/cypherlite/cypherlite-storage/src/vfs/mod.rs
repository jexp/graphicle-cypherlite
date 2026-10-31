//! Synchronous virtual filesystem for WAL, snapshots, and CDC (ADR-0072).

#[cfg(not(target_arch = "wasm32"))]
mod bundle;
#[cfg(not(target_arch = "wasm32"))]
mod host;
mod memory;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod native;

use std::fmt::Write as _;
use std::io::{self, SeekFrom, Write};

use cypherlite_core::{Error, Result};

#[cfg(not(target_arch = "wasm32"))]
pub use bundle::BundleVfs;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use bundle::HostAdmit;
pub use memory::MemoryVfs;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::NativeLease;
#[cfg(not(target_arch = "wasm32"))]
pub use native::NativeVfs;

pub fn is_bundle_path(path: &std::path::Path) -> bool {
    let raw = path.as_os_str().to_string_lossy();
    let trimmed = raw.trim_end_matches(['/', '\\']);
    std::path::Path::new(trimmed)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cl"))
}

pub struct VfsMeta {
    pub len: u64,
    pub is_dir: bool,
}

#[derive(Clone, Copy)]
pub struct VfsOpen {
    pub create: bool,
    /// Atomically create a new file, even if a dangling symlink occupies the name.
    pub create_new: bool,
    pub truncate: bool,
    pub append: bool,
    pub write: bool,
    pub read: bool,
}

impl VfsOpen {
    pub fn create_truncate() -> Self {
        Self {
            create: true,
            create_new: false,
            truncate: true,
            append: false,
            write: true,
            read: true,
        }
    }

    pub fn append_create() -> Self {
        Self {
            create: true,
            create_new: false,
            truncate: false,
            append: true,
            write: true,
            read: false,
        }
    }

    pub fn read_only() -> Self {
        Self {
            create: false,
            create_new: false,
            truncate: false,
            append: false,
            write: false,
            read: true,
        }
    }

    pub fn read_write() -> Self {
        Self {
            create: false,
            create_new: false,
            truncate: false,
            append: false,
            write: true,
            read: true,
        }
    }

    pub fn create_new() -> Self {
        Self {
            create: false,
            create_new: true,
            truncate: false,
            append: false,
            write: true,
            read: true,
        }
    }
}

pub trait Vfs: Send + Sync {
    fn durable(&self) -> bool;
    fn create_dir_all(&self, path: &str) -> Result<()>;
    /// Create parents for an atomic write, persisting new directory entries if needed.
    /// Durable implementations must override this fail-closed default.
    fn create_dir_all_for_atomic(&self, path: &str) -> Result<()> {
        if self.durable() {
            return Err(Error::storage(
                "durable VFS must implement create_dir_all_for_atomic",
            ));
        }
        self.create_dir_all(path)
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>>;
    fn exists(&self, path: &str) -> Result<bool>;
    fn metadata(&self, path: &str) -> Result<VfsMeta>;
    fn open(&self, path: &str, opts: VfsOpen) -> Result<Box<dyn VfsFile>>;
    /// `None` means the name was occupied; the existing object was not opened.
    /// Implementations of `open` must honor `create_new` atomically. Native
    /// overrides this fallback to recognize dangling symlink collisions too.
    fn open_exclusive(&self, path: &str) -> Result<Option<Box<dyn VfsFile>>> {
        match self.open(path, VfsOpen::create_new()) {
            Ok(file) => Ok(Some(file)),
            Err(_) if self.exists(path)? => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn remove_file(&self, path: &str) -> Result<()>;
    /// Remove several files as one logical batch. The default removes each
    /// path in turn; durable implementations may publish the whole batch with
    /// a single catalog generation and barrier pair (Bundle). Callers validate
    /// every path before calling.
    fn remove_files(&self, paths: &[String]) -> Result<()> {
        for path in paths {
            self.remove_file(path)?;
        }
        Ok(())
    }
    fn remove_dir_all(&self, path: &str) -> Result<()>;
    fn rename(&self, from: &str, to: &str) -> Result<()>;
    /// Persist a renamed entry in this virtual directory. Durable implementations
    /// must override this fail-closed default (Bundle uses virtual barriers).
    fn sync_dir(&self, _path: &str) -> Result<()> {
        if self.durable() {
            Err(Error::storage("durable VFS must implement sync_dir"))
        } else {
            Ok(())
        }
    }

    /// Move this opener's `.cypherlite.writer` lease out, once.
    /// `None` for a read-only open, a non-native VFS, or a lease already taken.
    #[cfg(not(target_arch = "wasm32"))]
    fn take_writer_lease(&self) -> Option<crate::WriterLease> {
        None
    }
}

pub trait VfsFile: Send {
    fn write_all(&mut self, buf: &[u8]) -> Result<()>;
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()>;
    fn seek(&mut self, pos: SeekFrom) -> Result<u64>;
    fn sync_all(&mut self) -> Result<()>;
    /// Cheapest correct durability barrier (ADR-0077). Defaults to [`sync_all`].
    fn durability_barrier(&mut self) -> Result<()> {
        self.sync_all()
    }
    fn set_len(&mut self, len: u64) -> Result<()>;
    fn len(&self) -> Result<u64>;

    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    fn stream_position(&mut self) -> Result<u64> {
        self.seek(SeekFrom::Current(0))
    }
}

/// A [`Write`] adapter over a VFS file which fails before a write would
/// exceed `limit`. It keeps Parquet checkpoint candidates bounded without
/// collecting their complete contents in heap memory.
#[cfg(any(test, parquet_codec))]
pub(crate) struct CappedVfsWriter<'a> {
    file: &'a mut dyn VfsFile,
    bytes: u64,
    limit: u64,
    exceeded: bool,
    message: &'static str,
}

#[cfg(any(test, parquet_codec))]
impl<'a> CappedVfsWriter<'a> {
    pub(crate) fn new(file: &'a mut dyn VfsFile, limit: u64, message: &'static str) -> Self {
        Self {
            file,
            bytes: 0,
            limit,
            exceeded: false,
            message,
        }
    }

    pub(crate) fn exceeded(&self) -> bool {
        self.exceeded
    }
}

#[cfg(any(test, parquet_codec))]
impl Write for CappedVfsWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Some(total) = self.bytes.checked_add(buf.len() as u64) else {
            self.exceeded = true;
            return Err(io::Error::other(self.message));
        };
        if total > self.limit {
            self.exceeded = true;
            return Err(io::Error::other(self.message));
        }
        self.file
            .write_all(buf)
            .map_err(|error| io::Error::other(error.to_string()))?;
        self.bytes = total;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn virt_parent(path: &str) -> Option<&str> {
    let i = path.rfind('/')?;
    if i == 0 {
        None
    } else {
        Some(&path[..i])
    }
}

pub(crate) fn virt_join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else if name.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// 16 bytes from the OS CSPRNG. The single source of name/token randomness
/// for exclusive temp files, shard generations, pull-identity objects and
/// reset tombstones (ADR-0098/0087/0090). Failures share one storage error.
pub(crate) fn random_bytes_16() -> Result<[u8; 16]> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| Error::storage(format!("OS randomness: {e}")))?;
    Ok(bytes)
}

/// Render 16 bytes as 32 lowercase hex characters.
pub(crate) fn hex_16(bytes: [u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

/// 128-bit OS-random token as lowercase hex.
pub(crate) fn random_hex_16() -> Result<String> {
    Ok(hex_16(random_bytes_16()?))
}

/// Exactly 32 lowercase hexadecimal characters, the token shape that
/// [`hex_16`] and [`random_hex_16`] produce.
#[cfg_attr(not(parquet_codec), allow(dead_code))]
pub(crate) fn is_lower_hex_32(text: &str) -> bool {
    text.len() == 32
        && text
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

const TEMP_ATTEMPTS: usize = 8;

fn temp_path(path: &str) -> Result<String> {
    Ok(format!("{path}.tmp-{}", random_hex_16()?))
}

fn cleanup_temp(vfs: &dyn Vfs, tmp: &str, original: Error) -> Error {
    // A failing rename can have published the inode already (notably Bundle).
    // Never remove the destination; touch the temp only if it still exists.
    let cleanup = vfs
        .exists(tmp)
        .and_then(|exists| if exists { vfs.remove_file(tmp) } else { Ok(()) });
    match cleanup {
        Ok(()) => original,
        Err(e) => Error::io(format!("{original}; temporary file cleanup failed: {e}")),
    }
}

/// Write to an exclusive, OS-random temp, sync and close it, rename, then
/// persist the containing directory on Native Unix (ADR-0098).
///
/// The handle is scoped so it is provably closed before the rename.
/// Two Windows rules force this exact shape (POSIX tolerates both
/// violations, so only the structure pins them):
///  * `sync_all` needs a write handle: FlushFileBuffers requires
///    GENERIC_WRITE, so a read-only open + `sync_all` fails with
///    "Access is denied" (os error 5) — hence create/truncate.
///  * renaming a file that still has an open handle fails with
///    os error 5 — hence the scope ends before `rename`.
pub fn atomic_write(vfs: &dyn Vfs, path: &str, bytes: &[u8]) -> Result<()> {
    atomic_write_stream(vfs, path, |file| file.write_all(bytes))
}

/// Produce a replacement directly into an exclusive temporary file. The
/// producer finishes (and drops any source handles it owns) before rename.
/// Before publication, errors retain the original destination and clean up
/// the temporary file; directory-sync errors occur after publication.
pub(crate) fn atomic_write_stream(
    vfs: &dyn Vfs,
    path: &str,
    produce: impl FnOnce(&mut dyn VfsFile) -> Result<()>,
) -> Result<()> {
    let parent = virt_parent(path).unwrap_or("");
    vfs.create_dir_all_for_atomic(parent)?;
    for _ in 0..TEMP_ATTEMPTS {
        let tmp = temp_path(path)?;
        let Some(mut file) = vfs.open_exclusive(&tmp)? else {
            continue;
        };
        let written = produce(&mut *file).and_then(|()| file.sync_all());
        // In particular, Windows requires the write handle closed before rename.
        drop(file);
        if let Err(e) = written {
            return Err(cleanup_temp(vfs, &tmp, e));
        }
        if let Err(e) = vfs.rename(&tmp, path) {
            return Err(cleanup_temp(vfs, &tmp, e));
        }
        // Once published, never clean up or retry after a failed directory sync.
        return vfs.sync_dir(parent);
    }
    Err(Error::io(format!(
        "could not create an exclusive temporary file for {path} after {TEMP_ATTEMPTS} collisions"
    )))
}

/// A `Write` sink bounded by a byte `limit`.
///
/// The one shared shape for every bounded serializer in this crate: count the
/// bytes written, and on the first write that would cross `limit` mark
/// `exceeded` and return the configured error. Captured bytes go to a
/// caller-owned `Vec` ([`BoundedSink::capture`]).
///
/// `BoundedSink::capture_capped` additionally bounds the backing allocation
/// with `try_reserve_exact` and geometric growth, never asking the allocator
/// for more than `min(limit, capacity_cap)` (used by the Parquet shard writer).
pub(crate) struct BoundedSink<'a> {
    bytes: u64,
    limit: u64,
    output: Option<&'a mut Vec<u8>>,
    exceeded: bool,
    message: &'static str,
    capacity_cap: Option<u64>,
}

impl<'a> BoundedSink<'a> {
    /// Count bytes without retaining the serialized payload.
    pub(crate) fn count(limit: u64, message: &'static str) -> Self {
        Self {
            bytes: 0,
            limit,
            output: None,
            exceeded: false,
            message,
            capacity_cap: None,
        }
    }

    /// Append captured bytes into `output`, up to `limit`.
    pub(crate) fn capture(output: &'a mut Vec<u8>, limit: u64, message: &'static str) -> Self {
        Self {
            bytes: 0,
            limit,
            output: Some(output),
            exceeded: false,
            message,
            capacity_cap: None,
        }
    }

    pub(crate) fn exceeded(&self) -> bool {
        self.exceeded
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
}

#[cfg(parquet_codec)]
impl<'a> BoundedSink<'a> {
    /// Append into `output` while keeping its capacity below
    /// `min(limit, capacity_cap)` with `try_reserve_exact` growth.
    #[cfg(test)]
    pub(crate) fn capture_capped(
        output: &'a mut Vec<u8>,
        limit: u64,
        capacity_cap: u64,
        message: &'static str,
    ) -> Self {
        Self {
            capacity_cap: Some(capacity_cap),
            ..Self::capture(output, limit, message)
        }
    }

    /// Backing capacity of the capture buffer.
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.output.as_ref().map_or(0, |out| out.capacity())
    }

    /// Bytes captured so far.
    #[cfg(test)]
    pub(crate) fn captured_len(&self) -> usize {
        self.output.as_ref().map_or(0, |out| out.len())
    }
}

impl Write for BoundedSink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let message = self.message;
        let Some(total) = self.bytes.checked_add(buf.len() as u64) else {
            self.exceeded = true;
            return Err(io::Error::other(message));
        };
        if total > self.limit {
            self.exceeded = true;
            return Err(io::Error::other(message));
        }
        if let Some(out) = self.output.as_mut() {
            let result = match self.capacity_cap {
                None => {
                    out.extend_from_slice(buf);
                    Ok(())
                }
                Some(cap) => append_bounded(out, buf, cap.min(self.limit), message),
            };
            if let Err(error) = result {
                self.exceeded = true;
                return Err(error);
            }
        }
        self.bytes = total;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Grow `out` by `buf` without ever asking the allocator for more than
/// `limit` total capacity. `try_reserve_exact` reserves relative to `len`, so
/// growth is geometric but clamped to the cap.
fn append_bounded(
    out: &mut Vec<u8>,
    buf: &[u8],
    limit: u64,
    message: &'static str,
) -> io::Result<()> {
    let envelope_error = || io::Error::other(message);
    let limit = usize::try_from(limit).map_err(|_| envelope_error())?;
    if out.capacity() > limit {
        return Err(envelope_error());
    }
    let end = out
        .len()
        .checked_add(buf.len())
        .filter(|&end| end <= limit)
        .ok_or_else(envelope_error)?;
    if end > out.capacity() {
        let target = out.capacity().max(1).saturating_mul(2).max(end).min(limit);
        out.try_reserve_exact(target - out.len())
            .map_err(io::Error::other)?;
        // An allocator may provide more capacity than requested. Do not rely
        // on shrinking it to enforce the envelope.
        if out.capacity() > limit {
            return Err(envelope_error());
        }
    }
    out.extend_from_slice(buf);
    Ok(())
}

pub fn read_to_vec(vfs: &dyn Vfs, path: &str) -> Result<Vec<u8>> {
    let mut f = vfs.open(path, VfsOpen::read_only())?;
    let len = f.len()? as usize;
    let mut buf = vec![0u8; len];
    if len > 0 {
        f.read_exact(&mut buf)?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
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

    struct FaultVfs<'a> {
        inner: &'a dyn Vfs,
        events: Arc<Mutex<Vec<&'static str>>>,
        fail: Option<&'static str>,
        fail_cleanup: bool,
        collision: Option<CollisionHook<'a>>,
        collided: Mutex<Option<String>>,
    }

    impl<'a> FaultVfs<'a> {
        fn new(inner: &'a dyn Vfs, fail: Option<&'static str>) -> Self {
            Self {
                inner,
                events: Arc::new(Mutex::new(Vec::new())),
                fail,
                fail_cleanup: false,
                collision: None,
                collided: Mutex::new(None),
            }
        }

        fn event(&self, stage: &'static str) -> Result<()> {
            self.events.lock().unwrap().push(stage);
            if self.fail == Some(stage) || (stage == "cleanup" && self.fail_cleanup) {
                Err(Error::io(format!("injected {stage} failure")))
            } else {
                Ok(())
            }
        }

        fn trace(&self) -> Vec<&'static str> {
            self.events.lock().unwrap().clone()
        }
    }

    struct FaultFile {
        inner: Option<Box<dyn VfsFile>>,
        events: Arc<Mutex<Vec<&'static str>>>,
        fail: Option<&'static str>,
    }

    impl Drop for FaultFile {
        fn drop(&mut self) {
            drop(self.inner.take());
            self.events.lock().unwrap().push("close");
        }
    }

    impl VfsFile for FaultFile {
        fn write_all(&mut self, buf: &[u8]) -> Result<()> {
            self.events.lock().unwrap().push("write");
            if self.fail == Some("write") {
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
            self.events.lock().unwrap().push("file_sync");
            if self.fail == Some("file_sync") {
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
            self.event("parents")?;
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
            self.event("exclusive_open")?;
            if self.fail == Some("collision") {
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
                        fail: self.fail,
                    }) as Box<dyn VfsFile>
                })
            })
        }
        fn remove_file(&self, path: &str) -> Result<()> {
            self.event("cleanup")?;
            self.inner.remove_file(path)
        }
        fn remove_dir_all(&self, path: &str) -> Result<()> {
            self.inner.remove_dir_all(path)
        }
        fn rename(&self, from: &str, to: &str) -> Result<()> {
            self.event("rename")?;
            self.inner.rename(from, to)?;
            if self.fail == Some("post_rename_error") {
                return Err(Error::io("injected post_rename_error failure"));
            }
            Ok(())
        }
        fn sync_dir(&self, path: &str) -> Result<()> {
            self.event("dir_sync")?;
            self.inner.sync_dir(path)
        }
    }

    fn atomic_faults(vfs: &dyn Vfs) {
        atomic_write(vfs, "nested/object", b"old").unwrap();
        for stage in ["parents", "exclusive_open"] {
            let fault = FaultVfs::new(vfs, Some(stage));
            assert!(atomic_write(&fault, "nested/object", b"new")
                .unwrap_err()
                .to_string()
                .contains(stage));
            assert_eq!(read_to_vec(vfs, "nested/object").unwrap(), b"old");
            assert_eq!(vfs.list_dir("nested").unwrap(), ["object"]);
        }
        for stage in ["write", "file_sync", "rename", "dir_sync"] {
            let fault = FaultVfs::new(vfs, Some(stage));
            let error = atomic_write(&fault, "nested/object", b"new").unwrap_err();
            assert!(error.to_string().contains(stage), "{error}");
            assert_eq!(
                read_to_vec(vfs, "nested/object").unwrap(),
                if stage == "dir_sync" { b"new" } else { b"old" }
            );
            assert_eq!(vfs.list_dir("nested").unwrap(), ["object"]);
            let trace = fault.trace();
            assert!(trace.starts_with(&["parents", "exclusive_open", "write"]));
            assert!(trace.contains(&"close"));
            if stage == "dir_sync" {
                assert_eq!(
                    trace,
                    [
                        "parents",
                        "exclusive_open",
                        "write",
                        "file_sync",
                        "close",
                        "rename",
                        "dir_sync"
                    ]
                );
            } else {
                assert_eq!(trace.last(), Some(&"cleanup"));
                atomic_write(vfs, "nested/object", b"old").unwrap();
            }
        }
        // Failed cleanup retains the original error and reports its own failure.
        let mut fault = FaultVfs::new(vfs, Some("rename"));
        fault.fail_cleanup = true;
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
        let fault = FaultVfs::new(vfs, Some("post_rename_error"));
        assert!(atomic_write(&fault, "nested/object", b"published")
            .unwrap_err()
            .to_string()
            .contains("post_rename_error"));
        assert_eq!(read_to_vec(vfs, "nested/object").unwrap(), b"published");
        assert_eq!(vfs.list_dir("nested").unwrap(), ["object"]);
        assert!(!fault.trace().contains(&"cleanup"));
    }

    fn atomic_collision(vfs: &dyn Vfs) {
        let mut fault = FaultVfs::new(vfs, None);
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
                .filter(|&&e| e == "exclusive_open")
                .count(),
            2
        );
        vfs.remove_file(&occupied).unwrap();

        let fault = FaultVfs::new(vfs, Some("collision"));
        let error = atomic_write(&fault, "collisions/value", b"other").unwrap_err();
        assert!(error.to_string().contains("after 8 collisions"));
        assert_eq!(read_to_vec(vfs, "collisions/value").unwrap(), b"new");
        assert_eq!(
            fault
                .trace()
                .iter()
                .filter(|&&event| event == "exclusive_open")
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
        assert!(vfs.metadata("nodes").unwrap().is_dir);
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
        let mut fault = FaultVfs::new(&vfs, None);
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
}

#[cfg(all(test, parquet_codec))]
mod sink_tests;
