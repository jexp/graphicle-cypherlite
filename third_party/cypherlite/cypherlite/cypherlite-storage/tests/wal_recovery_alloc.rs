//! An allocator-backed recovery regression: unlike a decoder-maintained count,
//! this sees decoded records retained in any Vec (including an uninstrumented one).
//! Each backend runs alone in its own test process, without RSS or parallel-test noise.

use std::alloc::{GlobalAlloc, Layout, System};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use cypherlite_core::Value;
use cypherlite_executor::{Graph, RedoOp};
use cypherlite_storage::{
    load_with_wal, memory_store_with_codec, parse_uri_with_options, SnapshotCodecKind, StoreTarget,
    Wal,
};

struct CountingAllocator;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static MEASURING: AtomicBool = AtomicBool::new(false);

fn allocated(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    if MEASURING.load(Ordering::Relaxed) {
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

// SAFETY: System owns the allocations; counters only observe successful operations.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(ptr, layout);
    }

    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        let next = System.realloc(ptr, old, new_size);
        if !next.is_null() {
            if new_size > old.size() {
                allocated(new_size - old.size());
            } else {
                LIVE.fetch_sub(old.size() - new_size, Ordering::Relaxed);
            }
        }
        next
    }
}

#[derive(Debug)]
struct Footprint {
    before: usize,
    peak: usize,
    after: usize,
}

fn measure<T>(run: impl FnOnce() -> T) -> (T, Footprint) {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    MEASURING.store(true, Ordering::Relaxed);
    let value = run();
    MEASURING.store(false, Ordering::Relaxed);
    let footprint = Footprint {
        before,
        peak: PEAK.load(Ordering::Relaxed),
        after: LIVE.load(Ordering::Relaxed),
    };
    (value, footprint)
}

const FRAMES: usize = 1_500;
const BODY_BYTES: usize = 8 * 1024;
// Large enough for graph bookkeeping and one decoded frame; far below 1,500
// owned payloads (~12 MiB). Compare to *live* bytes as well as the baseline:
// a temporary Vec dropped before return must still fail the high-water check.
const TRANSIENT_BUDGET: usize = 2 * 1024 * 1024;

fn run_case(backend: &str) {
    let dir = tempfile::tempdir().unwrap();
    let store: StoreTarget = match backend {
        "memory" => memory_store_with_codec(SnapshotCodecKind::Json).unwrap(),
        "native" => parse_uri_with_options(
            dir.path().join("db").to_str().unwrap(),
            None,
            Some(SnapshotCodecKind::Json),
        )
        .unwrap(),
        _ => panic!("unexpected backend"),
    };
    let mut wal = Wal::open(store.vfs(), "wal", 0).unwrap();
    let body: std::sync::Arc<str> = "x".repeat(BODY_BYTES).into();
    for id in 0..FRAMES {
        // The large, valid redo payload targets a missing node and is discarded
        // by the graph. This keeps the live graph small, so a buffered replay
        // cannot hide behind the memory needed for the final graph state.
        wal.append_record(
            id as u64 + 1,
            &[
                RedoOp::CreateNode {
                    id: id as i64,
                    labels: Vec::new().into(),
                    properties: std::sync::Arc::new(cypherlite_core::PropertyMap::new()),
                },
                RedoOp::SetNodeProp {
                    id: FRAMES as i64 + 1,
                    key: "discarded".into(),
                    value: Value::String(body.clone()),
                },
            ],
        )
        .unwrap();
        if id == FRAMES / 2 - 1 {
            wal.sync().unwrap();
            wal.rotate().unwrap();
        }
    }
    wal.sync().unwrap();
    drop(wal);

    let (wal, opened) = measure(|| Wal::open(store.vfs(), "wal", 0).unwrap());
    assert_eq!(wal.lsn, FRAMES as u64);
    assert!(
        opened.peak.saturating_sub(opened.before) < TRANSIENT_BUDGET,
        "{backend} Wal::open retained decoded frames: {opened:?}"
    );

    let (graph, loaded) = measure(|| load_with_wal(&store, &wal).unwrap());
    assert_eq!(graph.live_node_count(), FRAMES);
    assert_eq!(graph.commit_ts(), FRAMES as u64);
    assert!(graph.node((FRAMES - 1) as i64).is_some());
    assert!(graph.node(FRAMES as i64 + 1).is_none());
    assert!(
        loaded.peak.saturating_sub(loaded.after) < TRANSIENT_BUDGET,
        "{backend} load_with_wal retained a temporary record Vec: {loaded:?}"
    );
    drop(graph);

    // Negative control: reproduce the former load path (replay into Vec,
    // clone its ops into the graph). The same allocator must catch its
    // short-lived decoded-record accumulation even after that Vec is dropped.
    let (buffered_graph, buffered) = measure(|| {
        let graph = Graph::default();
        for rec in wal.replay(0).unwrap() {
            graph.apply_redo(rec.ops.clone(), rec.commit_ts).unwrap();
        }
        graph.rebuild_indexes();
        graph
    });
    assert_eq!(buffered_graph.live_node_count(), FRAMES);
    assert!(
        buffered.peak.saturating_sub(buffered.after) > FRAMES * BODY_BYTES / 2,
        "{backend} negative control failed to detect buffered recovery: {buffered:?}"
    );
    drop(buffered_graph);

    // The public explicit replay remains a materializing API. Its allocation
    // high-water and *retained* allocation must contrast with recovery above.
    let (records, replayed) = measure(|| wal.replay(0).unwrap());
    assert_eq!(records.len(), FRAMES);
    assert!(
        replayed.peak.saturating_sub(replayed.before) > FRAMES * BODY_BYTES / 2
            && replayed.after.saturating_sub(replayed.before) > FRAMES * BODY_BYTES / 2,
        "{backend} explicit replay must retain decoded payloads: {replayed:?}"
    );
}

#[test]
fn recovery_allocation_is_bounded_on_native_and_memory() {
    for backend in ["memory", "native"] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "measured_recovery_child", "--nocapture"])
            .env("CYPHERLITE_WAL_MEASURE_VFS", backend)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{backend} recovery allocation child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn measured_recovery_child() {
    if let Ok(backend) = std::env::var("CYPHERLITE_WAL_MEASURE_VFS") {
        run_case(&backend);
    }
}
