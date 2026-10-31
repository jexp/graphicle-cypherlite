//! Local CSV capability (ADR-0081, ADR-0108). A local target is opened and
//! then verified against the physical identity recorded at validation, so a
//! concurrent filesystem mutation between inspection and open cannot
//! substitute the file that is read.
//!
//! The module splits into [`policy`] (the validated local-file capability:
//! source classification, root confinement, the re-verified read) and
//! [`parse`] (RFC 4180 record/field splitting and row values); this root
//! keeps the per-statement [`CsvReader`] memoization that composes them.

mod parse;
mod policy;

#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use cypherlite_core::Result;

pub(crate) use parse::{field_terminator_char, record_value, split_csv_line, split_csv_records};
pub use policy::CsvPolicy;
#[cfg(test)]
pub(crate) use policy::DEFAULT_MAX_BYTES;

/// CIP-202's canned `artists.csv` is a dialect fixture, not a local-file
/// capability or a network fetch: the TCK/CIP scenarios name this exact URL
/// and expect these rows, while every other remote source stays denied by
/// policy (ADR-0081).
const CIP202_ARTISTS_URL: &str = "https://data.neo4j.com/bands/artists.csv";
const CIP202_ARTISTS_CSV: &str = "1,ABBA,1992\n2,Roxette,1986\n3,Europe,1979\n";

/// One memo per statement, shared across its nested clauses even when the
/// execution view is reused for subsequent statements.
pub(crate) struct CsvReader<'a> {
    policy: &'a CsvPolicy,
    source_memo: RefCell<HashMap<String, Arc<str>>>,
    canonical_memo: RefCell<HashMap<PathBuf, Arc<str>>>,
    #[cfg(test)]
    reads: Cell<usize>,
}

impl<'a> CsvReader<'a> {
    pub(crate) fn new(policy: &'a CsvPolicy) -> Self {
        let reader = Self {
            policy,
            source_memo: RefCell::new(HashMap::new()),
            canonical_memo: RefCell::new(HashMap::new()),
            #[cfg(test)]
            reads: Cell::new(0),
        };
        reader.seed_canned_sources();
        reader
    }

    /// Start a top-level statement on this view. Internal seeded execution
    /// keeps both memos intact for the duration of that statement.
    pub(crate) fn begin_statement(&self) {
        self.source_memo.borrow_mut().clear();
        self.canonical_memo.borrow_mut().clear();
        self.seed_canned_sources();
    }

    /// Seed the canned dialect fixtures into the source memo, so `load`
    /// keeps one uniform flow — memo hit, then the confinement policy —
    /// with no fixture check bolted onto the read path. The canned data is
    /// immutable and never a physical read.
    fn seed_canned_sources(&self) {
        self.source_memo
            .borrow_mut()
            .insert(CIP202_ARTISTS_URL.to_owned(), Arc::from(CIP202_ARTISTS_CSV));
    }

    pub(crate) fn load(&self, source: &str) -> Result<Arc<str>> {
        if let Some(text) = self.source_memo.borrow().get(source) {
            return Ok(text.clone());
        }
        let target = self.policy.target(source)?;
        let cached = self.canonical_memo.borrow().get(&target.path).cloned();
        let text = if let Some(text) = cached {
            text
        } else {
            #[cfg(test)]
            self.reads.set(self.reads.get() + 1);
            let text: Arc<str> = self.policy.read(&target)?.into();
            self.canonical_memo
                .borrow_mut()
                .insert(target.path, text.clone());
            text
        };
        self.source_memo
            .borrow_mut()
            .insert(source.to_owned(), text.clone());
        Ok(text)
    }

    /// The physical reads this reader performed — asserted by the
    /// engine-level `LOAD CSV` tests in `exec::call::tests`.
    #[cfg(test)]
    pub(crate) fn read_count(&self) -> usize {
        self.reads.get()
    }
}

/// A fresh temporary directory per test; cleanup is drop-based
/// (`TempDir`), so no test needs a remove_dir_all tail. Shared by the
/// reader/policy tests here and the engine-level `LOAD CSV` tests in
/// `exec::call::tests`.
#[cfg(test)]
pub(crate) fn test_root() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// The `file:///…` URL spelling of an absolute path, without introducing a
/// URL authority or Windows backslashes. Same audience as [`test_root`].
#[cfg(test)]
pub(crate) fn local_file_url(path: &std::path::Path) -> String {
    let path = path.to_str().unwrap().replace('\\', "/");
    format!("file:///{}", path.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::policy::unavailable;
    use super::*;
    use std::fs;

    #[test]
    fn reader_denies_remote_urls_with_or_without_root() {
        let dir = test_root();
        let root = dir.path();
        for policy in [
            CsvPolicy::default(),
            CsvPolicy::new(Some(root.to_path_buf()), None).unwrap(),
        ] {
            let reader = CsvReader::new(&policy);
            // The CIP-202 canned fixture is the one remote URL that loads.
            assert_eq!(
                reader
                    .load("https://data.neo4j.com/bands/artists.csv")
                    .unwrap()
                    .as_ref(),
                "1,ABBA,1992\n2,Roxette,1986\n3,Europe,1979\n"
            );
            for source in [
                "https://other.example/data.csv",
                "http://other.example/data.csv",
                "https://data.neo4j.com/bands/other.csv",
            ] {
                assert_eq!(reader.load(source).unwrap_err(), unavailable());
            }
            assert_eq!(reader.read_count(), 0);
        }
    }

    #[test]
    fn source_memo_survives_removal_within_view() {
        let dir = test_root();
        let root = dir.path();
        let path = root.join("rows.csv");
        fs::write(&path, "original\n").unwrap();
        let policy = CsvPolicy::new(Some(root.to_path_buf()), None).unwrap();
        let reader = CsvReader::new(&policy);

        let first = reader.load("rows.csv").unwrap();
        fs::remove_file(path).unwrap();
        let second = reader.load("rows.csv").unwrap();
        assert_eq!(second.as_ref(), "original\n");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(reader.read_count(), 1);

        let fresh = CsvReader::new(&policy);
        assert_eq!(fresh.load("rows.csv").unwrap_err(), unavailable());
        assert_eq!(fresh.read_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn source_memo_survives_in_root_symlink_retarget() {
        use std::os::unix::fs::symlink;

        let dir = test_root();
        let root = dir.path();
        fs::write(root.join("first.csv"), "first\n").unwrap();
        fs::write(root.join("second.csv"), "second\n").unwrap();
        let alias = root.join("alias.csv");
        symlink("first.csv", &alias).unwrap();
        let policy = CsvPolicy::new(Some(root.to_path_buf()), None).unwrap();
        let reader = CsvReader::new(&policy);

        let first = reader.load("alias.csv").unwrap();
        fs::remove_file(&alias).unwrap();
        symlink("second.csv", &alias).unwrap();
        let second = reader.load("alias.csv").unwrap();
        assert_eq!(second.as_ref(), "first\n");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(reader.read_count(), 1);

        let fresh = CsvReader::new(&policy);
        assert_eq!(fresh.load("alias.csv").unwrap().as_ref(), "second\n");
        assert_eq!(fresh.read_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn source_memo_survives_escape_retarget_but_fresh_view_denies() {
        use std::os::unix::fs::symlink;

        let dir = test_root();
        let root = dir.path();
        let outside_dir = test_root();
        let outside = outside_dir.path();
        fs::write(root.join("inside.csv"), "inside\n").unwrap();
        fs::write(outside.join("outside.csv"), "outside\n").unwrap();
        let alias = root.join("alias.csv");
        symlink("inside.csv", &alias).unwrap();
        let policy = CsvPolicy::new(Some(root.to_path_buf()), None).unwrap();
        let reader = CsvReader::new(&policy);

        let first = reader.load("alias.csv").unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(outside.join("outside.csv"), &alias).unwrap();
        let second = reader.load("alias.csv").unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(second.as_ref(), "inside\n");
        assert_eq!(reader.read_count(), 1);

        let fresh = CsvReader::new(&policy);
        assert_eq!(fresh.load("alias.csv").unwrap_err(), unavailable());
        assert_eq!(fresh.read_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn distinct_sources_share_canonical_memo_and_arc() {
        use std::os::unix::fs::symlink;

        let dir = test_root();
        let root = dir.path();
        fs::write(root.join("rows.csv"), "shared\n").unwrap();
        symlink("rows.csv", root.join("alias.csv")).unwrap();
        let policy = CsvPolicy::new(Some(root.to_path_buf()), None).unwrap();
        let reader = CsvReader::new(&policy);

        let first = reader.load("rows.csv").unwrap();
        let alias = reader.load("alias.csv").unwrap();
        let url = reader
            .load(&local_file_url(&root.join("rows.csv")))
            .unwrap();
        assert!(Arc::ptr_eq(&first, &alias));
        assert!(Arc::ptr_eq(&first, &url));
        assert_eq!(reader.read_count(), 1);
    }

    #[test]
    fn failed_source_is_retryable_after_creation() {
        let dir = test_root();
        let root = dir.path();
        let policy = CsvPolicy::new(Some(root.to_path_buf()), None).unwrap();
        let reader = CsvReader::new(&policy);

        assert_eq!(reader.load("later.csv").unwrap_err(), unavailable());
        assert_eq!(reader.read_count(), 0);
        fs::write(root.join("later.csv"), "created\n").unwrap();
        assert_eq!(reader.load("later.csv").unwrap().as_ref(), "created\n");
        assert_eq!(reader.read_count(), 1);
    }
}
