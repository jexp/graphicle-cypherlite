# Pre-ADR-0097 default WAL fixture

`pre_adr0084_default_wal/` was written by `cypherlite-storage` from
`origin/main` at commit `615306d` (before directory leases/typed `.lock`
markers). It is a Native directory with the JSON snapshot codec: Ada (node 0,
`:Person {name: 'Ada', age: 37}`) was checkpointed at LSN 1; Bob (node 1,
`:Person {name: 'Bob', age: 42}`) was then appended at LSN 2. Neither directory
ever had a `.lock` file. The first WAL frame is retained despite the checkpoint
so the single `00000001.log` contains both commits. The storage test checks
the full tree, snapshot, frame contents/CRC, recovered nodes, and byte stability.

To reproduce, check out `615306d` in a temporary worktree and place this
temporary example in `cypherlite/cypherlite-storage/examples/make_fixture.rs`:

```rust
use std::collections::BTreeMap;
use cypherlite_core::Value;
use cypherlite_executor::Graph;
use cypherlite_storage::{checkpoint_with_lsn, parse_uri_with_options, SnapshotCodecKind, Wal};

fn main() {
    let root = std::env::args().nth(1).expect("fixture output directory");
    let target = parse_uri_with_options(&root, None, Some(SnapshotCodecKind::Json)).unwrap();
    let mut wal = Wal::open(target.vfs(), "wal", 0).unwrap();
    let graph = Graph::default();
    for (name, age) in [("Ada", 37), ("Bob", 42)] {
        graph.begin_write();
        graph.create_node(vec!["Person".into()], BTreeMap::from([
            ("name".into(), Value::String(name.into())),
            ("age".into(), Value::Int(age)),
        ]));
        let ops = graph.take_redo();
        let ts = graph.pending_commit_ts();
        graph.abort_write();
        let lsn = wal.append_commit(ts, &ops).unwrap();
        graph.apply_redo(ops, ts).unwrap();
        if name == "Ada" {
            checkpoint_with_lsn(&target, &graph, lsn).unwrap();
        }
    }
    assert_eq!(wal.lsn, 2);
}
```

Run `cargo run -p cypherlite-storage --example make_fixture -- /path/to/output`.
The three generated files have SHA-256 hashes (no trailing newline in
`graph.json`):

| File | SHA-256 |
| --- | --- |
| `manifest.json` | `60d30020722f47559e07799f4190e4d1123899664be3c3f6af416620f0b8307e` |
| `graph.json` | `a0d298e730f7b2642a7216a941cb9942707f65ddc1ce120df28920712029a1b3` |
| `wal/00000001.log` | `8f04a007abe1c84c3e4841f284ecc764c6a864f95a5b95cb4c905bc0351e953c` |
