//! Shared test-support helpers for the CypherLite workspace.
//!
//! Test tooling only: crates pull this in as a dev-dependency, and the
//! workspace's own test harnesses (the TCK, query-latency,
//! graph-examples) consume it as a regular dependency because they are
//! themselves test tooling. Nothing here ships in a released artifact.

#[cfg(feature = "bolt")]
pub mod bolt;
pub mod git;
#[cfg(any(feature = "wire", feature = "loopback"))]
pub mod loopback;
#[cfg(feature = "nested_eval")]
pub mod nested_eval;
pub mod sampling;

/// The stack size of the big-stack test worker: 64 MiB, mirroring the
/// per-query worker stack the engine provisions (ADR-0259). The ADR-0257
/// depth tests recurse ~500 levels of parsing/lowering/validation, which
/// overflows the default 2 MiB test-thread stack in debug builds.
pub const BIG_STACK_BYTES: usize = 64 * 1024 * 1024;

/// The nearest-rank percentile index over `len` samples (an ascending
/// slice): `ceil(p * len) - 1`, clamped into range. The one definition of
/// the nearest-rank policy for the workspace's timing suites —
/// query-latency's percentile set and the graph-examples differential's
/// per-dataset stats both index through it, so the rank rule cannot drift
/// between suites.
///
/// # Panics
///
/// Panics if `len` is 0 — there is no rank over an empty sample set; the
/// callers guarantee non-empty (sorted sample vectors), and the assert
/// names the invariant instead of std's cryptic `min > max` clamp panic.
pub fn nearest_rank(len: usize, p: f64) -> usize {
    debug_assert!(len > 0, "nearest_rank needs at least one sample");
    (((p * len as f64).ceil() as usize).clamp(1, len)) - 1
}

/// Run `f` on a freshly spawned thread with a `stack_bytes` stack and return
/// its result.
///
/// Panics if the thread cannot be spawned; a panic inside `f` is observed
/// across the join and re-raised on the calling thread, so a failing test
/// body fails the test instead of being swallowed.
pub fn run_with_stack<T: Send + 'static>(
    stack_bytes: usize,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    std::thread::Builder::new()
        .stack_size(stack_bytes)
        .spawn(f)
        .expect("failed to spawn test thread")
        .join()
        .expect("test thread panicked")
}

/// Run `f` on a thread with a [`BIG_STACK_BYTES`] stack and return its
/// result: the harness the parser/front-end/engine depth tests and the TCK
/// runner share for recursion deeper than the default 2 MiB test-thread
/// stack survives in debug builds.
pub fn run_with_big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    run_with_stack(BIG_STACK_BYTES, f)
}

/// Write `bytes` to `path`, creating any missing parent directories first.
///
/// Fixture helper shared by tests that declare a filesystem tree by its file
/// paths; panics on any I/O error so a broken fixture fails the test at the
/// point of setup.
pub fn write_file(path: &std::path::Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

/// The engine URI for a filesystem path in tests; test tempdir paths are
/// valid UTF-8, so a non-UTF-8 path is a broken fixture and panics.
pub fn uri(path: &std::path::Path) -> &str {
    path.to_str().unwrap()
}

/// Lowercase hex of `bytes`, two digits per byte, no separators — the one
/// rendering the testing crates compare sha256 digests and byte-array wire
/// values with.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Walk `root` recursively in sorted entry order, collecting `leaf(path,
/// rel)` for every file: `rel` is the `/`-joined path relative to `root`.
/// A missing `root` yields an empty vector, so a store that was never
/// created compares equal to another absent store.
pub fn walk_tree<T>(
    root: &std::path::Path,
    mut leaf: impl FnMut(&std::path::Path, &str) -> T,
) -> Vec<T> {
    fn walk<T>(
        dir: &std::path::Path,
        prefix: &str,
        out: &mut Vec<T>,
        leaf: &mut impl FnMut(&std::path::Path, &str) -> T,
    ) {
        let mut ents: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|ent| ent.unwrap())
            .collect();
        ents.sort_by_key(|ent| ent.file_name());
        for ent in ents {
            let name = ent.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            if ent.file_type().unwrap().is_dir() {
                walk(&ent.path(), &rel, out, leaf);
            } else {
                out.push(leaf(&ent.path(), &rel));
            }
        }
    }
    let mut out = Vec::new();
    if root.exists() {
        walk(root, "", &mut out, &mut leaf);
    }
    out
}

/// Runs `f` on drop: a test hook is cleared even when the test body panics,
/// so a failing test cannot leak its hook into the next test on the thread.
pub struct ClearOnDrop<F: FnMut()>(F);

impl<F: FnMut()> ClearOnDrop<F> {
    pub fn new(f: F) -> Self {
        Self(f)
    }
}

impl<F: FnMut()> Drop for ClearOnDrop<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}

/// Restores an environment variable on drop: [`EnvRestore::new`] captures
/// the variable's current value (`var_os`), and every mutation the test
/// makes while the guard lives — panic path included, a failing assert
/// must not leak the mutation to later tests — is undone when it drops.
/// The one env guard for tests that read the process environment, which
/// parallel tests share.
pub struct EnvRestore {
    var: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvRestore {
    /// Captures `var`'s current value for [`Drop`] to restore.
    pub fn new(var: &'static str) -> Self {
        Self {
            var,
            previous: std::env::var_os(var),
        }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.var, value),
            None => std::env::remove_var(self.var),
        }
    }
}

/// Raw HTTP WebSocket upgrade request bytes; `extra_headers` is inserted
/// before the terminating blank line. The one spelling of the request a
/// WebSocket-serving test drives by hand (cypherlite-bolt's unit tests via
/// its testutil, its e2e roots via tests/common).
pub fn ws_upgrade_request(key: &str, extra_headers: &str) -> String {
    format!(
        "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n{extra_headers}\r\n"
    )
}

/// A `CYPHERLITE_`-prefixed process knob: the prefix is the membership
/// rule, so every product knob named later is covered without a list to
/// keep in sync. Private to this module — the rule is an implementation
/// detail of [`harness_child_env`].
fn is_cypherlite_knob(key: &std::ffi::OsStr) -> bool {
    key.as_encoded_bytes().starts_with(b"CYPHERLITE_")
}

/// The harness child env policy for every spawn of a product binary from
/// test tooling — the `cypherlite` CLI the graph-examples pulls spawn and
/// the serve children the datasets loadtests spawn: scrub every ambient
/// `CYPHERLITE_*` knob by rule, so the child is configured only by its
/// argv and the explicit env a caller sets after this call, and opt the
/// child out of telemetry (ADR-0079 §6: a harness spawn is not product
/// usage, so a CI or loadtest run never emits events).
pub fn harness_child_env(cmd: &mut std::process::Command) {
    for (key, _) in std::env::vars_os() {
        if is_cypherlite_knob(&key) {
            cmd.env_remove(&key);
        }
    }
    cmd.env("DO_NOT_TRACK", "1");
}

/// Path to the built `cypherlite` binary, from `CYPHERLITE_BIN` — the
/// one knob every test harness that spawns the real binary uses, since
/// a `testing/` crate cannot reach `CARGO_BIN_EXE_cypherlite` (only
/// `cypherlite-cli`'s own tests see it). `None` when the variable is
/// unset or does not point at a file (the same file-ness contract the
/// CLI e2e harness's `binary_path` enforces for the same knob); the
/// caller decides fail-loud (the agent A/B harness) or
/// self-skip (graph-examples) with [`cypherlite_bin_missing_message`].
pub fn cypherlite_bin() -> Option<std::path::PathBuf> {
    let raw = std::env::var_os("CYPHERLITE_BIN").map(std::path::PathBuf::from)?;
    if raw.is_file() {
        return Some(raw);
    }
    // The documented value is workspace-relative (target/release/cypherlite);
    // a test binary's cwd is its crate root, so resolve against the
    // workspace root — a testing/<crate>/ manifest dir is two levels
    // below it.
    let manifest_relative = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(&raw);
    manifest_relative.is_file().then_some(manifest_relative)
}

/// How to provide the binary when [`cypherlite_bin`] comes back empty.
pub fn cypherlite_bin_missing_message() -> String {
    "CYPHERLITE_BIN is unset or does not point at a built cypherlite binary; \
     run `cargo build --release -p cypherlite-cli` and set \
     CYPHERLITE_BIN=target/release/cypherlite (workspace-relative or absolute)"
        .to_string()
}

/// One percentile over any ordered metric, through the canonical
/// [`nearest_rank`] rule (`p` is a fraction: 0.25, 0.50, 0.75) — the
/// one rank policy every timing suite in the workspace indexes through
/// (query-latency's p50s, graph-examples' per-dataset stats, the
/// agent A/B harness's p25/median/p75 distributions), so the
/// even-count choice cannot drift between suites.
///
/// # Panics
///
/// Panics when `values` is empty, when the values are not mutually
/// comparable (a NaN among them), or when `p` is not a fraction
/// (a percentile spelled 50 instead of 0.50 — the trap that once made
/// a "median" report maxima, so this helper makes it loud).
pub fn percentile<T: PartialOrd + Clone>(values: &[T], p: f64) -> T {
    assert!(
        (0.0..=1.0).contains(&p),
        "percentile takes a fraction (0.50), not a percentage (50)"
    );
    assert!(!values.is_empty(), "percentile of a non-empty slice");
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("percentile values are ordered"));
    sorted[nearest_rank(sorted.len(), p)].clone()
}

#[cfg(test)]
mod percentile_tests {
    use super::*;

    /// The fraction contract: the median of a small odd slice is its
    /// middle, and a percentile spelled 50 (not 0.50) fails the
    /// fraction assert loudly — the exact trap that once made a
    /// "median" report maxima, pinned so it cannot return silently.
    #[test]
    fn percentiles_are_fractions_not_percentages() {
        let values = [1, 2, 100];
        assert_eq!(percentile(&values, 0.50), 2);
        assert_eq!(percentile(&values, 0.25), 1);
        assert_eq!(percentile(&values, 0.75), 100);
        let trap = std::panic::catch_unwind(|| percentile(&values, 50.0));
        assert!(
            trap.is_err(),
            "a percentage must fail the fraction assert, not clamp to the max"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_with_big_stack_returns_the_body_result() {
        assert_eq!(run_with_big_stack(|| 41 + 1), 42);
    }

    #[test]
    fn run_with_stack_propagates_a_body_panic() {
        let result = std::panic::catch_unwind(|| {
            run_with_stack(2 * 1024 * 1024, || panic!("body boom"));
        });
        let payload = result.unwrap_err();
        let msg = payload.downcast_ref::<String>();
        assert!(
            msg.is_some_and(|m| m.contains("test thread panicked")),
            "join error must be re-raised, got: {msg:?}"
        );
    }

    #[test]
    fn write_file_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c.txt");
        write_file(&path, b"hi");
        assert_eq!(std::fs::read(&path).unwrap(), b"hi");
    }

    #[test]
    fn uri_returns_the_path_str() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(uri(dir.path()), dir.path().to_str().unwrap());
    }

    #[test]
    fn hex_is_lowercase_two_digits_per_byte() {
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }

    /// The env policy opts every harness child out of telemetry
    /// (ADR-0079 §6), whatever the ambient opt-out state.
    #[test]
    fn harness_child_env_opts_out_of_telemetry() {
        let mut cmd = std::process::Command::new("cypherlite");
        harness_child_env(&mut cmd);
        let opt_out = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("DO_NOT_TRACK"))
            .and_then(|(_, v)| v);
        assert_eq!(opt_out, Some(std::ffi::OsStr::new("1")));
    }

    /// The scrub membership rule is the `CYPHERLITE_` prefix — a product
    /// knob is covered the day it is named, and nothing else is touched.
    #[test]
    fn the_knob_rule_is_the_cypherlite_prefix() {
        assert!(is_cypherlite_knob(std::ffi::OsStr::new(
            "CYPHERLITE_SNAPSHOT_FORMAT"
        )));
        assert!(is_cypherlite_knob(std::ffi::OsStr::new(
            "CYPHERLITE_IDENTITY"
        )));
        assert!(!is_cypherlite_knob(std::ffi::OsStr::new("DO_NOT_TRACK")));
        assert!(!is_cypherlite_knob(std::ffi::OsStr::new("RUST_BACKTRACE")));
    }

    #[test]
    fn walk_tree_is_sorted_recursive_and_tolerates_a_missing_root() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("b/one.txt"), b"1");
        write_file(&dir.path().join("a/two.txt"), b"22");
        write_file(&dir.path().join("top.txt"), b"");
        let tree = walk_tree(dir.path(), |path, rel| {
            (rel.to_owned(), std::fs::read(path).unwrap())
        });
        assert_eq!(
            tree,
            vec![
                ("a/two.txt".to_owned(), b"22".to_vec()),
                ("b/one.txt".to_owned(), b"1".to_vec()),
                ("top.txt".to_owned(), Vec::new()),
            ]
        );
        assert!(walk_tree(&dir.path().join("missing"), |_, rel| rel.to_owned()).is_empty());
    }

    #[test]
    fn clear_on_drop_runs_the_reset_even_on_panic() {
        let cleared = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&cleared);
        let result = std::panic::catch_unwind(|| {
            let _guard = ClearOnDrop::new(|| flag.store(true, std::sync::atomic::Ordering::SeqCst));
            panic!("body boom");
        });
        assert!(result.is_err());
        assert!(cleared.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn env_restore_puts_the_captured_value_back_on_drop() {
        // A test-only name no other test in this binary touches: the
        // process environment is shared across the parallel tests.
        const VAR: &str = "CYPHERLITE_TEST_SUPPORT_ENV_RESTORE_PROBE";
        std::env::remove_var(VAR);
        {
            let _guard = EnvRestore::new(VAR);
            std::env::set_var(VAR, "set");
            assert_eq!(
                std::env::var_os(VAR).as_deref(),
                Some(std::ffi::OsStr::new("set"))
            );
        }
        assert!(
            std::env::var_os(VAR).is_none(),
            "a captured absence is restored"
        );
        std::env::set_var(VAR, "prior");
        {
            let _guard = EnvRestore::new(VAR);
            std::env::set_var(VAR, "changed");
        }
        assert_eq!(
            std::env::var_os(VAR).as_deref(),
            Some(std::ffi::OsStr::new("prior")),
            "a captured value is restored"
        );
        // The panic path restores too — a failing assert must not leak
        // the mutation to later tests.
        let result = std::panic::catch_unwind(|| {
            let _guard = EnvRestore::new(VAR);
            std::env::set_var(VAR, "panic-set");
            panic!("body boom");
        });
        assert!(result.is_err());
        assert!(
            std::env::var_os(VAR).as_deref() == Some(std::ffi::OsStr::new("prior")),
            "the panic path restores too"
        );
        std::env::remove_var(VAR);
    }
}
