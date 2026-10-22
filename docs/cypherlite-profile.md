# Profiling the graph store with `PROFILE`

CypherLite's `PROFILE` returns a plan tree with per-operator `Rows`,
`DbHits` and `TimeNs`. On a query against a populated store it is the
cheapest way to tell an index seek from a full label scan, and it is how
the three version indexes in `store_graph.rs` were found. This note records
the rules it revealed, with the measurements, so the next query can be
judged without re-deriving them.

## Reading a plan

```
cypherlite query --read-only --database <store>/reviews.graph --mode jsonl --max-rows 0 \
  "PROFILE MATCH (v:Live {node_id:'x'}) RETURN v.data AS d"
```

The operator *name* is not the signal — every node access renders as
`NodeByLabelScan`, whether it scanned the label or seeked an index. **`DbHits`
is the signal**, and so is how the numbers move with the label's size:

- `DbHits` ≈ label cardinality → full label scan, O(label) per call.
- `DbHits` ≈ 1 per row (or 0 with the rows found by index) → index seek.

A quick confirmation when in doubt: grow the label and re-run. A seek is
flat, a scan grows linearly.

## Rules

**1. A single-property map seeks; a multi-property map does not.**
`MATCH (n:L {p: $v})` and `WHERE n.p = $v` use the index on `(L, p)` — and
a composite index serves each of its columns, so `{sha256: $s}` seeks even
when the index is declared on `(repository_id, sha256)`. But
`MATCH (n:L {a: $a, b: $b})` — the form graphicle's hot queries use
throughout — never seeks, *even when a composite index covers both
properties*. Measured on a 200k-node label with a unique index on
`(repo, key)`:

| form | 200k | 600k |
|---|---|---|
| `{repo:'r1', key:'k'}` | 1.98 ms | 10.9 ms |
| `WHERE key='k'` | 0.058 ms | 0.037 ms |

The two-property form is O(label); the one-property form is O(1). At
graphicle's current label sizes (167 files, 2971 chunks) the difference is
invisible, which is why this is a recorded follow-up and not a rewrite: at
a 100k-file repository the map form is what would dominate. If it is ever
rewritten, the rule is to keep the most selective property in the map and
move the rest to `WHERE`.

**2. Do not "fix" a query by anchoring it on the indexed label.**
Given `MATCH (s:Symbol {repository_id:$r})-[:HAS_VERSION]->(v:Live {node_id:$n})`
the planner starts at the *indexed* end and expands backwards, once the
index exists — 1 db hit. Rewriting the anchor onto that label with the
scope moved into `WHERE`
(`MATCH (v:Live {node_id:$n}) WHERE v.repository_id = $r`) makes it *worse*:
a full label scan, because the multi-property-plus-`WHERE` shape loses the
seek. The planner picks the indexed start by itself; leave the pattern
alone and give it the index.

**3. Index maintenance is cheap here.** 100k version creates: 0.57 s with
the index, 0.54 s without. 100k label flips: 0.98 s vs 0.92 s. A 20k
`MERGE` + version create was *faster* with the indexes present (2.53 s vs
3.78 s) — the planner had a better start available. Budget ~5-7% on writes
and measure reads before believing otherwise.

## What the indexes bought

`Live(node_id)`, `Live(owner)` and `SymbolVersion(node_id)`, on a store
with 5276 symbols:

| query shape | without | with |
|---|---|---|
| close a live version (per `UNWIND` row) | 5276 dbHits, 10.2 ms | **1 dbHit, 0.022 ms** |
| read a version by node id | 5276 dbHits, 10.5 ms | **1 dbHit, 0.049 ms** |
| live versions of a dirty owner group | 5276 dbHits, 10.0 ms | **30 dbHits, 0.099 ms** |

The projection does the first of these once per row of its closing
`UNWIND` and the third once per dirty owner group, so the effect is
per-file rather than per-sync.

## Compile profile matters more than anything else here

The same 12,000-file sync test (`core_limits::twelve_thousand_small_files_sync`):

| build | engine | wall |
|---|---|---|
| release | cypherlite main | **140 s** |
| release | main, indexes disabled | 146 s |
| debug | v0.12.2 (the previous pin) | 900 s |
| debug | main | >58 min (killed) |

Two conclusions. First, the indexes are not a regression in any profile
(140 s vs 146 s). Second, **debug is the wrong profile for this workload**
— release is 6.4x faster than the old debug baseline — and the engine's
debug build regressed further between v0.12.2 and main on this specific
path (a 167-file `graphicle setup` is unaffected: 376.8 s on main vs
388.8 s on v0.12.2, both debug). Run heavy suites in release.

## Reproducing

```
# a populated store to profile
mkdir -p /tmp/glprof/proj && cp -R crates/graphicle/src /tmp/glprof/proj/
cd /tmp/glprof/proj && graphicle --data-dir /tmp/glprof/data setup --yes --clients none .

# the plan of any query
cypherlite query --read-only --database /tmp/glprof/data/reviews.graph --mode jsonl \
  "PROFILE MATCH (s:Symbol {repository_id:'<repo>'})-[:HAS_VERSION]->(v:Live) \
   WHERE v.owner = 'src/agent_detect.rs' RETURN v.data AS d LIMIT 1"
```
