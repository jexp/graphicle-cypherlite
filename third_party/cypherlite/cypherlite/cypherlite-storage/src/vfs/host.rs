use std::fs::File;

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
