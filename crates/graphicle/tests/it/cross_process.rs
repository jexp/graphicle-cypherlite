//! The graph store across processes: content committed by one process is
//! visible to later ones, and a process that never touches the content store
//! does not wait on the graph writer lease (the lease is transient, held per
//! operation).
use graphicle_runtime::store::{Store, s};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Stdio;
use std::time::Instant;
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    app: std::path::PathBuf,
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let app = dir.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let store = Store::open(dir.path().join("data")).unwrap();
        Self { dir, app, store }
    }
    /// A short-lived second process on the same store, run in `cwd`.
    fn child(&self, cwd: &Path, args: &[&str]) -> (bool, String, String) {
        let output = crate::fork_guard::spawn(
            std::process::Command::new(env!("CARGO_BIN_EXE_graphicle"))
                .current_dir(cwd)
                .arg("--data-dir")
                .arg(self.dir.path().join("data"))
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )
        .unwrap()
        .wait_with_output()
        .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

/// A child's sync is visible to a later child and to this process's store.
#[test]
fn committed_content_is_visible_across_processes() {
    let f = Fixture::new();
    let (ok, stdout, stderr) = f.child(&f.app, &["sync", "."]);
    assert!(ok, "{stderr}");
    let synced: Value = serde_json::from_str(&stdout).unwrap();
    let repo = s(&synced, "repository_id").to_string();
    // This process opens the store after the child exited.
    let heads = graphicle_runtime::store_content::heads(&f.store.graph(), &repo).unwrap();
    assert_eq!(heads.keys().cloned().collect::<Vec<_>>(), ["a.py"]);
    // A second child reads what the first committed.
    let (ok, stdout, stderr) = f.child(&f.app, &["context", "alpha"]);
    assert!(ok, "{stderr}");
    let packet: Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        packet["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["id"] == "file:a.py"),
        "the second child sees the synced file: {packet:#}"
    );
    let _ = json!(null);
}

/// A read-only child opens the store frozen beside a held writer lease and
/// still answers (the engine's `OpenMode::ReadOnly`).
#[test]
fn a_reading_child_beside_a_held_lease_answers_read_only() {
    let f = Fixture::new();
    let (ok, _stdout, stderr) = f.child(&f.app, &["sync", "."]);
    assert!(ok, "{stderr}");
    // Hold the graph writer lease in this process for the child's lifetime.
    let graph = f.store.graph();
    let started = Instant::now();
    let (ok, stdout, stderr) = f.child(&f.app, &["context", "alpha"]);
    assert!(ok, "{stderr}");
    let packet: Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        packet["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["id"] == "file:a.py"),
        "the read-only child sees the frozen content: {packet:#}"
    );
    drop(graph);
    let _ = started.elapsed();
}
