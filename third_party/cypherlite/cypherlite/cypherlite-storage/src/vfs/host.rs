use std::fs::File;
use std::path::Path;

use cypherlite_core::{Error, Result};

/// Cheapest correct durability barrier on a host `File` (ADR-0077).
pub(crate) fn host_durability_barrier(file: &File) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::fdatasync(file.as_raw_fd()) };
        if rc != 0 {
            return Err(Error::storage(format!(
                "fdatasync: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_BARRIERFSYNC) };
        if rc != 0 {
            return Err(Error::storage(format!(
                "F_BARRIERFSYNC: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        file.sync_all().map_err(|e| Error::storage(e.to_string()))
    }
}

/// Rename `from` onto `to` without *ever* replacing an existing `to` —
/// the host platform's exclusive-rename plumbing (Linux `renameat2`
/// `RENAME_NOREPLACE`, macOS `renamex_np` `RENAME_EXCL`, Windows
/// `MoveFileW`), the never-replace primitive reset rollback and every
/// "never replace" test share. A `to` occupied by anything — including a
/// dangling symlink — fails with the OS error; no existence precheck can
/// provide this. Targets without such a syscall fail closed with
/// `Unsupported`. The policy that calls this (which tree may move where)
/// stays with the caller; this is the platform mechanism only.
pub(crate) fn host_rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let from = CString::new(from.as_os_str().as_bytes())?;
        let to = CString::new(to.as_os_str().as_bytes())?;
        #[cfg(target_os = "linux")]
        // SAFETY: both CStrings own NUL-terminated paths for the duration of
        // the syscall; AT_FDCWD and RENAME_NOREPLACE are valid libc constants.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        // SAFETY: the owned CStrings remain alive during renamex_np, and the
        // exclusive-rename flag prevents replacing a new destination.
        let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            if rc == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (&from, &to);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "exclusive reset rename is unsupported on this target",
            ))
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        // MoveFileW does not replace an existing destination, unlike
        // std::fs::rename on Windows. Both UTF-16 buffers are NUL terminated.
        #[link(name = "Kernel32")]
        extern "system" {
            fn MoveFileW(from: *const u16, to: *const u16) -> i32;
        }
        let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: the owned UTF-16 buffers above are NUL-terminated and stay
        // alive for this call; MoveFileW does not retain their pointers.
        if unsafe { MoveFileW(from.as_ptr(), to.as_ptr()) } != 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}
