//! The validated local-file capability behind `LOAD CSV` (ADR-0081,
//! ADR-0108): source-spelling classification and normalization, root
//! confinement, and the re-verified read. A local target is opened and then
//! verified against the physical identity recorded at validation, so a
//! concurrent filesystem mutation between inspection and open cannot
//! substitute the file that is read.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use cypherlite_core::file_identity::{self, FileIdentity};
use cypherlite_core::{Error, Result};

pub(crate) const DEFAULT_MAX_BYTES: u64 = 8_388_608;
const MAX_BYTES_CEILING: u64 = 67_108_864;

/// Validated, immutable local-file capability for `LOAD CSV` (ADR-0081).
#[derive(Debug, Clone)]
pub struct CsvPolicy {
    root: Option<PathBuf>,
    max_bytes: u64,
}

impl Default for CsvPolicy {
    fn default() -> Self {
        Self::new(None, None).expect("default CSV policy is valid")
    }
}

pub(super) fn unavailable() -> Error {
    Error::execution("LOAD CSV file unavailable")
}

/// The one spelling of the `CsvPolicy::new` denial: an out-of-range byte
/// limit or a root that is not an existing directory.
fn invalid_config() -> Error {
    Error::semantic("LOAD CSV invalid configuration")
}

/// A canonical local target together with the physical identity observed when
/// it was validated. `read` requires the opened handle to match it (ADR-0108).
#[derive(Debug)]
pub(super) struct VerifiedTarget {
    pub(super) path: PathBuf,
    identity: FileIdentity,
}

/// Canonicalize `candidate`, returning it only when it resolves to a regular
/// file that stays inside the canonical `root`, together with the physical
/// identity observed at validation (ADR-0108). Any canonicalization,
/// inspection or confinement failure is a denial, never an OS error.
fn confined(candidate: PathBuf, root: &Path) -> Option<VerifiedTarget> {
    let target = candidate.canonicalize().ok()?;
    if !target.starts_with(root) {
        return None;
    }
    // Fail closed against a final-component symlink at inspection time; the
    // identity recorded here is required again on the opened handle.
    if !std::fs::symlink_metadata(&target).ok()?.is_file() {
        return None;
    }
    let identity = file_identity::of_path(&target).ok()?;
    Some(VerifiedTarget {
        path: target,
        identity,
    })
}

/// The drive-rooted path of a local `file:///` URL, or a denial: the URL
/// must spell `/<drive>:/<rest>` with forward slashes — never a UNC/device
/// prefix or a drive-relative path, which Windows path normalization would
/// otherwise reinterpret.
#[cfg(windows)]
fn windows_drive_path(url_path: &str) -> Result<&str> {
    let drive_path = &url_path[1..];
    let bytes = drive_path.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || bytes[2] != b'/'
        || drive_path.contains('\\')
        || drive_path[3..].contains(':')
    {
        return Err(unavailable());
    }
    Ok(drive_path)
}

impl CsvPolicy {
    /// Validate the byte limit and canonicalize an existing CSV directory.
    /// A missing root denies local files, even when a limit is configured.
    pub fn new(root: Option<PathBuf>, max_bytes: Option<u64>) -> Result<Self> {
        let max_bytes = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
        if !(1..=MAX_BYTES_CEILING).contains(&max_bytes) {
            return Err(invalid_config());
        }
        let root = root
            .map(|dir| {
                let canonical = dir.canonicalize().map_err(|_| invalid_config())?;
                if !canonical.is_dir() {
                    return Err(invalid_config());
                }
                Ok(canonical)
            })
            .transpose()?;
        Ok(Self { root, max_bytes })
    }

    /// The configured per-file byte limit.
    #[cfg(test)]
    pub(crate) fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// The canonical CSV root, when local files are enabled.
    #[cfg(test)]
    pub(crate) fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    pub(super) fn target(&self, source: &str) -> Result<VerifiedTarget> {
        let root = self.root.as_ref().ok_or_else(unavailable)?;
        // One resolution rule for every spelling: each classified
        // candidate, in order, must canonicalize to a regular file confined
        // to the root (`confined`); anything else is a denial.
        Self::classify(source, root)?
            .into_iter()
            .find_map(|candidate| confined(candidate, root))
            .ok_or_else(unavailable)
    }

    /// Classify a `LOAD CSV` source spelling into its candidate paths, in
    /// resolution order, so [`CsvPolicy::target`] applies exactly one
    /// resolution rule to every spelling:
    ///
    /// - `file:///absolute/path` is the only accepted URL form (no
    ///   authority, `?`, `#` or `%`; on Windows a drive-rooted
    ///   forward-slash path via `windows_drive_path`). Its candidates are
    ///   the absolute path as spelled, then the root-relative reading — so
    ///   the URL and bare-relative spellings of one in-root file resolve to
    ///   the same canonical target, while the absolute reading still wins
    ///   when it names a regular file under the root. Either reading is
    ///   canonicalized and confined to the root.
    /// - A bare path is rejected with a scheme, a `?`/`#`, a root or prefix,
    ///   or a parent component; otherwise its one candidate is the path
    ///   joined to `root`.
    fn classify(source: &str, root: &Path) -> Result<Vec<PathBuf>> {
        if let Some(url_path) = source.strip_prefix("file://") {
            // The only accepted URL form is file:///absolute/path. In
            // particular, file://localhost and file:////host are rejected.
            if !url_path.starts_with('/')
                || url_path.starts_with("//")
                || url_path.contains(['?', '#', '%'])
            {
                return Err(unavailable());
            }
            #[cfg(windows)]
            let url_path = windows_drive_path(url_path)?;
            let path = Path::new(url_path);
            if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
                return Err(unavailable());
            }
            // A `file:///name.csv` URL is also read as a path relative to
            // the configured root. The path is known absolute here: a Unix
            // absolute path strips its leading `/`; a Windows drive-rooted
            // URL path (`C:/…`) is absolute without that prefix, so it is
            // used as-is.
            let relative = path.strip_prefix("/").unwrap_or(path);
            Ok(vec![path.to_path_buf(), root.join(relative)])
        } else {
            let scheme = source.split_once(':').is_some_and(|(prefix, _)| {
                prefix.starts_with(|c: char| c.is_ascii_alphabetic())
                    && prefix
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
            });
            if scheme || source.contains(['?', '#']) {
                return Err(unavailable());
            }
            let path = Path::new(source);
            if path.as_os_str().is_empty()
                || path.has_root()
                || path
                    .components()
                    .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
            {
                return Err(unavailable());
            }
            Ok(vec![root.join(path)])
        }
    }

    pub(super) fn read(&self, target: &VerifiedTarget) -> Result<String> {
        let mut opts = OpenOptions::new();
        opts.read(true);
        // Never follow a symlink substituted at the final component (ADR-0108):
        // on Unix the open fails, on Windows the link itself is opened and the
        // identity comparison below denies it.
        file_identity::nofollow(&mut opts);
        let file = opts.open(&target.path).map_err(|_| unavailable())?;
        if file_identity::of_file(&file).map_err(|_| unavailable())? != target.identity
            || !file.metadata().map_err(|_| unavailable())?.is_file()
        {
            return Err(unavailable());
        }
        let mut bytes = Vec::new();
        file.take(self.max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| unavailable())?;
        if bytes.len() as u64 > self.max_bytes {
            return Err(Error::execution("LOAD CSV file exceeds byte limit"));
        }
        String::from_utf8(bytes).map_err(|_| unavailable())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{local_file_url, test_root};
    use super::*;
    use std::fs;

    /// `CsvPolicy::new`'s own validation: the byte-limit range and the
    /// existing-directory root rule. The Graph-side surface (defaults,
    /// `configure_load_csv`, capability preservation) is covered in
    /// `graph::tests`.
    #[test]
    fn new_validates_byte_limit_and_root() {
        let dir = test_root();
        let root = dir.path();
        let file = root.join("rows.csv");
        fs::write(&file, "safe\n").unwrap();
        for limit in [0, 67_108_865, u64::MAX] {
            assert_eq!(
                CsvPolicy::new(Some(root.to_path_buf()), Some(limit)).unwrap_err(),
                invalid_config()
            );
        }
        for invalid_root in [root.join("missing"), file] {
            assert_eq!(
                CsvPolicy::new(Some(invalid_root), None).unwrap_err(),
                invalid_config()
            );
        }
        // A relative root is accepted by the constructor but never stored
        // relative; neither callers nor Graph can manufacture an unchecked one.
        let relative = CsvPolicy::new(Some(PathBuf::from(".")), None).unwrap();
        assert!(relative.root.as_ref().unwrap().is_absolute());
        assert!(CsvPolicy::new(None, Some(MAX_BYTES_CEILING)).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn local_drive_urls_are_absolute_and_restricted_to_the_csv_root() {
        let dir = test_root();
        let root = dir.path();
        let inside = root.join("rows.csv");
        fs::write(&inside, "safe\n").unwrap();
        let outside = root.with_extension("outside.csv");
        fs::write(&outside, "private\n").unwrap();
        let policy = CsvPolicy::new(Some(root.to_path_buf()), None).unwrap();
        let url = local_file_url(&inside);
        assert_eq!(
            policy.target(&url).unwrap().path,
            inside.canonicalize().unwrap()
        );

        let root_url = local_file_url(&root);
        let outside_url = local_file_url(&outside);
        for source in [
            outside_url.as_str(),
            &format!("{root_url}/../outside.csv"),
            &format!(
                "{root_url}/../{}/rows.csv",
                root.file_name().unwrap().to_string_lossy()
            ),
            &url.replace("/rows.csv", "/rows%2ecsv"),
            &url.replace("/rows.csv", "/rows.csv:stream"),
            &format!("file:///{}", inside.display()),
            &format!("file://{}", inside.display()),
            "file://host/C:/rows.csv",
            "file://C:\\rows.csv",
            "file:////server/share/rows.csv",
            r"file:///\\server\share\rows.csv",
            r"file:///\\?\C:\rows.csv",
            r"file:///\\.\C:\rows.csv",
            "file:///C:rows.csv",
            "C:rows.csv",
            r"\rows.csv",
        ] {
            assert_eq!(
                policy.target(source).unwrap_err(),
                unavailable(),
                "{source}"
            );
        }
        fs::remove_file(outside).unwrap();
    }

    #[cfg(not(windows))]
    #[test]
    fn file_url_paths_resolve_against_the_policy_root() {
        let dir = test_root();
        let root = dir.path();
        let inside = root.join("rows.csv");
        fs::write(&inside, "safe\n").unwrap();
        let outside = root.with_extension("outside.csv");
        fs::write(&outside, "private\n").unwrap();
        let policy = CsvPolicy::new(Some(root.to_path_buf()), None).unwrap();
        let canonical = inside.canonicalize().unwrap();

        // The absolute URL of the in-root file and the root-relative
        // `file:///name.csv` spelling resolve to the same canonical target.
        assert_eq!(
            policy.target(&local_file_url(&inside)).unwrap().path,
            canonical
        );
        assert_eq!(policy.target("file:///rows.csv").unwrap().path, canonical);

        // Neither reading escapes the root or fabricates a missing file.
        for source in [
            "file:///missing.csv",
            "file:///../rows.csv",
            "file:///rows.csv/../rows.csv",
            "file:////rows.csv",
            "file://rows.csv",
            &local_file_url(&outside),
        ] {
            assert_eq!(
                policy.target(source).unwrap_err(),
                unavailable(),
                "{source}"
            );
        }
        fs::remove_file(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn read_denies_target_swapped_for_out_of_root_symlink() {
        use std::os::unix::fs::symlink;

        let dir = test_root();
        let root = dir.path();
        let outside_dir = test_root();
        let outside = outside_dir.path();
        fs::write(root.join("rows.csv"), "inside\n").unwrap();
        fs::write(outside.join("outside.csv"), "outside\n").unwrap();
        let policy = CsvPolicy::new(Some(root.to_path_buf()), None).unwrap();
        let target = policy.target("rows.csv").unwrap();

        fs::remove_file(root.join("rows.csv")).unwrap();
        symlink(outside.join("outside.csv"), root.join("rows.csv")).unwrap();
        assert_eq!(policy.read(&target).unwrap_err(), unavailable());
    }

    #[test]
    fn read_denies_target_swapped_for_other_in_root_file() {
        let dir = test_root();
        let root = dir.path();
        fs::write(root.join("rows.csv"), "one\n").unwrap();
        fs::write(root.join("other.csv"), "two\n").unwrap();
        let policy = CsvPolicy::new(Some(root.to_path_buf()), None).unwrap();
        let target = policy.target("rows.csv").unwrap();

        // The final component is a real regular file, so O_NOFOLLOW does not
        // refuse it; only the recorded identity catches the substitution.
        fs::remove_file(root.join("rows.csv")).unwrap();
        fs::rename(root.join("other.csv"), root.join("rows.csv")).unwrap();
        assert_eq!(policy.read(&target).unwrap_err(), unavailable());
    }

    #[cfg(unix)]
    #[test]
    fn read_denies_intermediate_directory_swap() {
        use std::os::unix::fs::symlink;

        let dir = test_root();
        let root = dir.path();
        let outside_dir = test_root();
        let outside = outside_dir.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/rows.csv"), "inside\n").unwrap();
        fs::write(outside.join("rows.csv"), "outside\n").unwrap();
        let policy = CsvPolicy::new(Some(root.to_path_buf()), None).unwrap();
        let target = policy.target("sub/rows.csv").unwrap();

        // The final component is a real regular file, so O_NOFOLLOW does not
        // refuse it; only the recorded identity catches the substitution.
        fs::rename(root.join("sub"), root.join("sub-orig")).unwrap();
        symlink(outside, root.join("sub")).unwrap();
        assert_eq!(policy.read(&target).unwrap_err(), unavailable());
    }

    #[test]
    fn read_denies_mismatched_recorded_identity() {
        let dir = test_root();
        let root = dir.path();
        let file = root.join("rows.csv");
        fs::write(&file, "rows\n").unwrap();
        let other = root.join("other.csv");
        fs::write(&other, "other\n").unwrap();
        let policy = CsvPolicy::new(Some(root.to_path_buf()), None).unwrap();

        let target = VerifiedTarget {
            path: file.canonicalize().unwrap(),
            identity: file_identity::of_path(&other.canonicalize().unwrap()).unwrap(),
        };
        assert_eq!(policy.read(&target).unwrap_err(), unavailable());
    }
}
