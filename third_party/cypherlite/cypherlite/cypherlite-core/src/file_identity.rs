//! Physical file identity for storage ownership (ADR-0097) and LOAD CSV open-then-verify (ADR-0108).
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

#[cfg(windows)]
const FILE_READ_ATTRIBUTES: u32 = 0x80;
#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// A final-component symlink must not be followed between inspection and open
/// (ADR-0097, ADR-0108). Windows opens the reparse point itself and validates
/// the returned handle.
pub fn nofollow(opts: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(not(any(unix, windows)))]
    let _ = opts;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    volume: u64,
    index: u64,
}

pub fn of_path(path: &Path) -> io::Result<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(path)?;
        Ok(FileIdentity {
            volume: meta.dev(),
            index: meta.ino(),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        // FILE_READ_ATTRIBUTES, not GENERIC_READ. LockFileEx makes a second
        // handle's read of the locked range return ERROR_LOCK_VIOLATION;
        // attributes do not touch that range. FILE_FLAG_BACKUP_SEMANTICS
        // permits directory handles, and FILE_FLAG_OPEN_REPARSE_POINT
        // identifies the reparse point itself.
        opts.access_mode(FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        of_file(&opts.open(path)?)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file identity unavailable",
        ))
    }
}

pub fn of_file(file: &File) -> io::Result<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        Ok(FileIdentity {
            volume: meta.dev(),
            index: meta.ino(),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };

        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
        // SAFETY: File owns a valid OS handle throughout this call. The API
        // writes the complete info struct on success and retains no pointers;
        // we only assume initialization after a successful return.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful API call initialized every field of `info`.
        let info = unsafe { info.assume_init() };
        Ok(FileIdentity {
            volume: u64::from(info.dwVolumeSerialNumber),
            index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file identity unavailable",
        ))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn file_and_directory_identities_are_physical() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        let hard_link = tmp.path().join("hard-link");
        let other = tmp.path().join("other");
        std::fs::write(&file, b"same").unwrap();
        std::fs::hard_link(&file, &hard_link).unwrap();
        std::fs::write(&other, b"same").unwrap();
        assert_eq!(of_path(&file).unwrap(), of_path(&hard_link).unwrap());
        assert_eq!(
            of_file(&File::open(&file).unwrap()).unwrap(),
            of_path(&file).unwrap()
        );
        assert_ne!(of_path(&file).unwrap(), of_path(&other).unwrap());

        let one = tmp.path().join("one");
        let two = tmp.path().join("two");
        std::fs::create_dir(&one).unwrap();
        std::fs::create_dir(&two).unwrap();
        assert_ne!(of_path(&one).unwrap(), of_path(&two).unwrap());

        let junction = tmp.path().join("junction");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&one)
            .status()
            .unwrap();
        assert!(status.success());
        assert_ne!(of_path(&junction).unwrap(), of_path(&one).unwrap());
    }
}
