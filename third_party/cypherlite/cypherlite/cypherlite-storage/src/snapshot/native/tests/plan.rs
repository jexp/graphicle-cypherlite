//! Planning rewrite units over slots (ADR-0183 Decision 2): kept files that
//! hold more rows or bytes together than one file can stay apart, so the
//! halves of a file halved on an envelope miss are not merged again, and a
//! seed wider than `MAX_WINDOW` ids is cut at the page envelope rows.

use super::super::coalesce::{plan_units, Step};
use super::super::layout::Slot;
use super::super::*;
use super::harness::checkpoint_at;
use super::harness::*;
use crate::codec::parquet::{decoded_preflight, take_encoded_rows, take_measured_rows};
use crate::memory_store;
use cypherlite_core::Value;
use cypherlite_executor::Graph;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

/// What planning makes of the slots: a kept part's `[id_lo, id_hi)`, or a
/// unit's slot indexes and the `[start, end)` of each of its files.
#[derive(Debug, PartialEq)]
enum Planned {
    Keep(i64, i64),
    Unit(Range<usize>, Vec<(i64, i64)>),
}

/// Plan `slots` over `all_ids` around `dirty` for `kind` at `target`
/// bytes. The graph is empty, so no id costs decoded bytes: only the width,
/// the manifest and, for a wide seed, its live ids decide.
fn plan(kind: Kind, target: u64, slots: &[Slot], all_ids: &[i64], dirty: &[i64]) -> Vec<Planned> {
    let store = memory_store();
    let graph = Graph::default();
    let mut owned = Vec::new();
    let ctx = ShardWrite {
        target: &store,
        graph: &graph,
        ts: 1,
        kind,
        target_bytes: target,
        owned: &mut owned,
    };
    let steps = plan_units(&ctx, slots, all_ids, dirty).unwrap();
    steps
        .into_iter()
        .map(|step| match step {
            Step::Keep(part) => Planned::Keep(part.id_lo, part.id_hi),
            Step::Rewrite(unit) => {
                let files = unit.files.iter().map(|f| (f.start, f.end)).collect();
                Planned::Unit(unit.slots, files)
            }
        })
        .collect()
}

#[test]
fn kept_files_over_one_file_together_stay_apart() {
    let mid = 500_000;
    let pair = |rows, bytes| {
        [(0, mid), (mid, 2 * mid)].map(|(lo, hi)| Slot::Kept(node_part(lo, hi, rows, bytes)))
    };
    let merged = || vec![Planned::Unit(0..2, vec![(0, 2 * mid)])];
    // The left part seeds a unit, or the right one does.
    let apart = |dirty: i64| {
        if dirty < mid {
            vec![
                Planned::Unit(0..1, vec![(0, mid)]),
                Planned::Keep(mid, 2 * mid),
            ]
        } else {
            vec![
                Planned::Keep(0, mid),
                Planned::Unit(1..2, vec![(mid, 2 * mid)]),
            ]
        }
    };
    let target = 10_000;
    for dirty in [0, mid] {
        for (kind, rows, bytes, joins) in [
            // Two halves of the node page envelope's 699,008 rows fill it;
            // one row more passes it.
            (Kind::Node, 349_504, 5_000, true),
            (Kind::Node, 349_505, 5_000, false),
            // The relationship page envelope holds 419,392 rows.
            (Kind::Rel, 209_696, 5_000, true),
            (Kind::Rel, 209_697, 5_000, false),
            // The files' bytes together fill the target, or pass it.
            (Kind::Node, 1, target / 2, true),
            (Kind::Node, 1, target / 2 + 1, false),
        ] {
            let planned = plan(kind, target, &pair(rows, bytes), &[0, mid], &[dirty]);
            let expected = if joins { merged() } else { apart(dirty) };
            assert_eq!(planned, expected, "{kind:?} {rows} rows {bytes} bytes");
        }
    }
}

#[test]
fn wide_seed_is_cut_at_the_page_envelope_rows() {
    let ids: Vec<i64> = (0..1_500_000).collect();
    let wide = [Slot::Window(0..3_000_000)];
    let node_files = vec![(0, 699_008), (699_008, 1_398_016), (1_398_016, 3_000_000)];
    let planned = plan(Kind::Node, MAX_FILE, &wide, &ids, &[]);
    assert_eq!(planned, [Planned::Unit(0..1, node_files)]);
    let rel_files = vec![
        (0, 419_392),
        (419_392, 838_784),
        (838_784, 1_258_176),
        (1_258_176, 3_000_000),
    ];
    let planned = plan(Kind::Rel, MAX_FILE, &wide, &ids, &[]);
    assert_eq!(planned, [Planned::Unit(0..1, rel_files)]);

    // A seed of at most MAX_WINDOW ids is cut by bytes alone; a file of
    // more rows than the page envelope is halved when written.
    let narrow = [Slot::Window(0..MAX_WINDOW)];
    let planned = plan(Kind::Node, MAX_FILE, &narrow, &ids[..1_000_000], &[]);
    assert_eq!(planned, [Planned::Unit(0..1, vec![(0, MAX_WINDOW)])]);
}

/// `len` characters of a 64-letter alphabet drawn from `seed`. Snappy finds
/// few repeats in them and does no entropy coding, so they barely compress.
fn noise(seed: &mut u64, len: usize) -> String {
    const LETTERS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    (0..len)
        .map(|_| {
            *seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            char::from(LETTERS[(*seed >> 58) as usize])
        })
        .collect()
}

#[test]
fn halves_of_a_halved_file_stay_apart() {
    let store = memory_store();
    let graph = Graph::default();
    let mut seed = 1;
    let mut noisy = || {
        BTreeMap::from([(
            "text".to_owned(),
            Value::String(noise(&mut seed, 2000).into()),
        )])
    };
    graph.begin_write().unwrap();
    for _ in 0..8 {
        graph.create_node(vec!["Item".into()], noisy()).unwrap();
    }
    let ts = apply_commit(&graph);
    // The target is the eight rows' decoded bytes, so they are planned as
    // one file. Encoded, the noise and the file's own overhead pass the
    // target: the file misses its envelope once and is halved.
    let decoded = |ts| decoded_preflight(&graph, ts, &graph.all_node_ids_at(ts), Kind::Node);
    let target = decoded(ts).unwrap();
    take_encoded_rows();
    checkpoint_at(&store, &graph, 0, ts, target).unwrap();
    let halves = manifest(&store).node_parts;
    assert_eq!(ranges(&halves), [(0, 4), (4, 4096)]);
    assert_eq!(take_encoded_rows(), 8 + 4 + 4);
    assert!(halves[0].bytes + halves[1].bytes > target, "{halves:?}");

    // A write to either half rewrites that half alone. The two halves' live
    // rows still fit the target together; their files' bytes do not, so
    // the other half is ruled out before its rows are measured or read.
    for (half, id) in [(0, 1), (1, 6)] {
        let before = manifest(&store).node_parts;
        graph.begin_write().unwrap();
        graph.set_property_node(id, "text", Value::String(noise(&mut seed, 2000).into()));
        let ts = apply_commit(&graph);
        assert_eq!(decoded(ts).unwrap(), target);
        let (watched, opens) = watch_opens(&store, &before[half].path);
        take_measured_rows();
        take_encoded_rows();
        checkpoint_at(&watched, &graph, 0, ts, target).unwrap();
        assert_eq!(take_measured_rows(), 4, "half {half}");
        assert_eq!(take_encoded_rows(), 1, "half {half}");
        assert_eq!(opens.load(Ordering::SeqCst), 1, "half {half}");
        let parts = manifest(&store).node_parts;
        assert_eq!(ranges(&parts), ranges(&before));
        assert_eq!(parts[1 - half], before[1 - half]);
        assert_ne!(parts[half].path, before[half].path);
        assert_eq!(store.vfs.list_dir("nodes").unwrap().len(), 2);
        assert_reopens_as(&store, &graph);
    }
}
