//! The workspace's canonical platform process-memory samplers: one sampler
//! family shared by every harness that reports process RSS — the dataset
//! load tests, memory shapes, checkpoints, bulk load tests, and the
//! query-latency suite.
//!
//! Sampling is platform-gated whole-body, each sampler a set of
//! item-level cfg'd items (the `process_peak_rss_bytes` idiom): Linux
//! reads `/proc/<pid>/status`, macOS asks `task_info` and `getrusage`,
//! and everywhere else each sampler is a `None`-returning item with no
//! syscall compiled in, so an unsupported platform never runs a dead
//! cast or discards a result it cannot use. What a runner does with an
//! unavailable sampler is the runner's own policy: the dataset harnesses
//! guard with `require_sampling_available` and fail with one explicit
//! error instead of a fabricated zero-byte report, while the
//! query-latency suite records `peak_rss_bytes: null` and leaves judging
//! the absence to its compare step.

/// The samplers' platform-support set this module's item gates define:
/// `true` where at least one sampler compiles (Linux and macOS), `false`
/// where every sampler is the `None` item with no syscall compiled in.
/// The one spelling of the set — use sites consult this constant instead
/// of re-deriving the pair.
pub const SAMPLING_SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

/// RSS and high-water mark from `/proc/<pid>/status` (`VmRSS`, `VmHWM`).
///
/// Linux only: [`process_status`] returns `None` on other platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessStatus {
    pub rss_bytes: u64,
    pub peak_rss_bytes: u64,
}

/// RSS and high-water mark for `pid`, or `None` when no sample is
/// available — the platform has no sampler (only Linux reads `/proc`),
/// or the platform query failed.
#[cfg(target_os = "linux")]
pub fn process_status(pid: u32) -> Option<ProcessStatus> {
    read_proc_status(&format!("/proc/{pid}/status"))
}

/// RSS and high-water mark for `pid`, or `None` when no sample is
/// available — the platform has no sampler (only Linux reads `/proc`),
/// or the platform query failed.
#[cfg(not(target_os = "linux"))]
pub fn process_status(_pid: u32) -> Option<ProcessStatus> {
    None
}

/// Current process RSS in bytes, or `None` when no sample is available —
/// the platform has no sampler, or the platform query failed.
#[cfg(target_os = "linux")]
pub fn process_rss_bytes() -> Option<u64> {
    process_status(std::process::id()).map(|status| status.rss_bytes)
}

/// Current process RSS in bytes, or `None` when no sample is available —
/// the platform has no sampler, or the platform query failed.
#[cfg(target_os = "macos")]
pub fn process_rss_bytes() -> Option<u64> {
    macos_resident_size()
}

/// Current process RSS in bytes, or `None` when no sample is available —
/// the platform has no sampler, or the platform query failed.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_rss_bytes() -> Option<u64> {
    None
}

/// Process high-water RSS in bytes, or `None` when no sample is
/// available — the platform has no sampler, or the platform query
/// failed.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn process_peak_rss_bytes() -> Option<u64> {
    // SAFETY: `libc::rusage` is a POD struct of integer fields, so
    // all-zero bits are a valid value for the type.
    let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
    // SAFETY: `getrusage` only writes to the struct through the passed
    // pointer and reads no other memory; `RUSAGE_SELF` names this process.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 {
        return None;
    }
    let raw = usage.ru_maxrss as u64;
    Some(raw.saturating_mul(MAXRSS_UNIT_BYTES))
}

/// Process high-water RSS in bytes, or `None` when no sample is
/// available — the platform has no sampler, or the platform query
/// failed.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_peak_rss_bytes() -> Option<u64> {
    None
}

/// `ru_maxrss` is reported in KiB on Linux.
#[cfg(target_os = "linux")]
const MAXRSS_UNIT_BYTES: u64 = 1024;

/// `ru_maxrss` is reported in bytes on macOS.
#[cfg(target_os = "macos")]
const MAXRSS_UNIT_BYTES: u64 = 1;

#[cfg(target_os = "linux")]
fn read_proc_status(path: &str) -> Option<ProcessStatus> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut rss = None;
    let mut hwm = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            rss = parse_status_kb(rest);
        } else if let Some(rest) = line.strip_prefix("VmHWM:") {
            hwm = parse_status_kb(rest);
        }
    }
    // The all-or-nothing join never discards a good sample: the kernel
    // writes VmRSS and VmHWM in the same /proc/<pid>/status output block
    // (both, or neither when the process has no mm), and
    // kernel-formatted values always parse, so either both `?`s see a
    // value or the file itself failed to read.
    Some(ProcessStatus {
        rss_bytes: rss?,
        peak_rss_bytes: hwm?,
    })
}

#[cfg(target_os = "linux")]
fn parse_status_kb(rest: &str) -> Option<u64> {
    let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
    Some(kb.saturating_mul(1024))
}

#[cfg(target_os = "macos")]
fn macos_resident_size() -> Option<u64> {
    // SAFETY: `mach_task_basic_info` is a POD struct of integer fields,
    // so all-zero bits are a valid value; `mach_task_self` returns this
    // task's own port, valid for the call with no rights transfer; the
    // `MACH_TASK_BASIC_INFO` flavor matches the struct type behind the
    // cast pointer; and `count` is pre-set to `MACH_TASK_BASIC_INFO_COUNT`,
    // which `task_info` reads and updates in place.
    unsafe {
        let mut info: libc::mach_task_basic_info = std::mem::zeroed();
        let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
        #[allow(deprecated)]
        let task = libc::mach_task_self();
        let kr = libc::task_info(
            task,
            libc::MACH_TASK_BASIC_INFO,
            (&mut info as *mut libc::mach_task_basic_info).cast(),
            &mut count,
        );
        if kr == libc::KERN_SUCCESS {
            Some(info.resident_size)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn samplers_report_a_positive_rss_on_supported_platforms() {
        assert!(process_rss_bytes().is_some_and(|rss| rss > 0));
        assert!(process_peak_rss_bytes().is_some_and(|peak| peak > 0));
    }
}
