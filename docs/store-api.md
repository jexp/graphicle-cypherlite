# The graph store façade (`store_graph`)

`crates/graphicle/src/store_graph.rs` is the CypherLite-backed core the
`Store` façade is being re-cored onto (migration plan §1). It is additive:
the rusqlite paths in `store.rs` keep working while modules migrate one by
one. This page is the contract module rewrites code against. Engine:
neo4j-labs CypherLite **v0.12.2** (path dependency, `default-features =
false` — JSON snapshot codec, no arrow/parquet); behaviour references are
`docs/spike-cypherlite.md` (executed evidence) and
`cypherlite-migration.md` (design).

## Open modes

```rust
use graphicle_runtime::store_graph::GraphStore;

let store = GraphStore::open(directory.join("reviews.graph"))?; // durable directory store
let store = GraphStore::open_in_memory()?;                      // ephemeral, for tests/scratch
```

- `GraphStore::open(path)` opens (or creates) a CypherLite **directory
  store** (WAL + snapshots) at `path` — beside, not instead of,
  `reviews.sqlite3` until the migration completes. Symlink paths are
  refused. `GraphStore::path()` returns it. The engine's writer lease is
  exclusive, so a second open of the same path **in this process** shares
  the live engine through a per-process registry, and an open that finds the
  lease held by **another process** follows the store read-only (reads pin
  snapshots; writes fail with a clear error) instead of failing.
- `GraphStore::open_in_memory()` is the `:memory:` mode that replaces
  rusqlite's `open_in_memory` in tests and transient processing (staging,
  expansion scratch).
- `GraphStore` is cheap to clone; clones share one engine. The engine takes
  a single-process writer lease on a durable store.
- `store.checkpoint()` flushes the WAL into a snapshot (maintenance/backup
  paths); `store.database()` exposes the raw `cypherlite::Database` for
  snapshot/clone work.

## Connections: one per worker thread, never one per request

`store.connection()` checks out a `cypherlite::Connection`. A connection is
`Send` but `!Sync`, every query takes `&mut self`, and the first query
spawns a 64 MiB-stack worker thread (migration doc §6 item 6). **Check one
out per worker thread and keep it for the thread's lifetime.** The
`transaction`/`read` helpers below check out a connection per call and are
the right granularity for request handlers; code with its own long-lived
worker (the job runner) should hold one `Connection` and pass `&mut` to the
query helpers directly.

## Transactions and reads

```rust
let value = store.transaction(|db| {
    db.query_with_params("MERGE (m:Memory {id: $id}) SET m += $props", params)?;
    Ok(value)
})?;

let rows = store.read(|db| store_graph::rows(db, "MATCH (m:Memory) RETURN m", Params::new()))?;
```

- `transaction(f)` does `begin` → `f` → `commit`. A **write-write conflict
  at commit** (`cypherlite::Error::Conflict`, the engine's optimistic
  concurrency) reruns the whole closure, up to
  `store_graph::MAX_WRITE_ATTEMPTS` (3) attempts; the exhausted error
  surfaces as a 409 `Error::conflict`. Any other error rolls back (an
  engine transaction dropped uncommitted rolls back implicitly).
  Consequently `f` is `Fn`, not `FnOnce`: keep it free of side effects
  outside the transaction and safe to rerun. Holds over 2 s are logged with
  the caller's location when `GRAPHICLE_PHASE_TIMINGS=1`, mirroring the SQL
  store's slow-hold reporting.
- `read(f)` runs `f` on a fresh connection with **no write transaction** —
  the engine pins a consistent snapshot per statement/connection. Do not
  write inside `read`.
- Errors convert into the crate's `Error` (`From<cypherlite::Error>`):
  conflicts become 409s, everything else a 400 "Graph storage operation
  failed: …".

## Queries and parameters

```rust
use graphicle_runtime::store_graph::{Params, json_to_value, one, rows, s};

let mut params = Params::new(); // BTreeMap<String, cypherlite::Value>
params.insert("rows".into(), json_to_value(&serde_json::json!([
    {"id": "c1", "index_text": "rust borrow checker"},
])));
let found = rows(db, "UNWIND $rows AS row MERGE (c:Chunk {id: row.id}) SET c += row", params)?;
let first = one(db, "MATCH (c:Chunk {id: $id}) RETURN c.index_text AS text", params)?;
let text = s(&first.unwrap(), "text"); // "" when absent, like store::s
```

- Parameters are a `BTreeMap<String, cypherlite::Value>`
  (`store_graph::Params`); `json_to_value` converts a `serde_json::Value`
  (objects → maps, arrays → lists — `UNWIND $rows AS row MERGE … SET n +=
  row` is the bulk-upsert pattern). Dynamic labels and rel types are
  parameters too: `MATCH (n)-[r:$($rel_type)]->(m)`, `CREATE (n:$($labels))`.
- `rows`/`one` return `serde_json` objects keyed by column name — the
  module boundary stays `serde_json::Value` in/out, so callers of a
  rewritten module see no signature change. Row fields that are nodes come
  back as their properties plus `_id`/`_labels`; relationships add `_type`,
  `_start`, `_end`.
- Writes that must batch use plain `UNWIND $rows` inside one
  `transaction`. Do **not** reach for `CALL {} IN TRANSACTIONS` by default:
  the spike measured it ~300× slower than plain batched UNWIND on
  `:memory:` in v0.12.2 (spike E6).

## Schema at open (DDL, idempotent)

Every open applies the schema; a second open of the same store is a no-op
(`CREATE … IF NOT EXISTS` for indexes; re-declaring and re-promoting the
graph type is inert).

- **Graph type `graphicle`** (migration plan §2.6) with the
  spike-corrected v0.12.2 syntax: `=>` after the identifying label,
  `name IS TYPE` declarations, and composite keys as top-level
  `CONSTRAINT FOR … REQUIRE … IS KEY` elements (a `CONSTRAINT` inside a
  node element's parens parse-errors, and edge property blocks need
  `-[:TYPE => {…}]->` — both verified by the smoke tests). It is
  **promoted** (`ALTER CURRENT GRAPH TYPE SET GRAPH TYPE graphicle`); a
  declared-but-unpromoted type enforces nothing. After promotion keys,
  `NOT NULL` and property types are enforced; undeclared labels and
  properties stay open.
- **The graph type wins over hand-written constraints on the same label**
  (spike E2's either/or quirk): open first drops any of the spike's ten
  fallback constraints (`ws_id`, `cred_key`, `proj_key`, `obj_key`,
  `mem_id`, `job_key`, `sym_key`, `ep_key`, `run_key`, `del_key`) so a store
  that once carried them promotes cleanly. New code must not add
  constraints on graph-type-managed labels.
- **Indexes** (§2.7): range (`job_ready`, `sym_file`, `chunk_path`,
  `mem_lookup`) and fulltext (`chunk_text` on `Chunk.index_text`,
  `mem_text` on `Memory.title/body`, `doc_text` on `ConnectorDoc.content`).
  Fulltext indexes maintain themselves on writes — no triggers.
  Query them with `SEARCH n IN (FULLTEXT INDEX chunk_text FOR $q LIMIT $k)`.
- **Vector indexes are created lazily, not at open — deviation from §2.7.**
  A vector index fixes its dimension at creation, and graphicle's embedding
  dimension is a property of the embedding model (`model_key`), only known
  when a store first publishes vectors. The vector pipeline therefore calls
  `store.ensure_vector_indexes(dimensions)` (idempotent; cosine) when it
  knows the model, creating `chunk_embed` (`Chunk.embedding`), `mem_embed`
  (`Memory.embedding`) and `ent_embed` (`Entity.embedding`). Query with
  `SEARCH n IN (VECTOR INDEX chunk_embed FOR $qv LIMIT $k) SCORE AS score`.
  Other deviations from the §2.6 sketch: `SymbolVersion` declares its
  properties minimal (`data IS STRING` payload) because the versioned
  property set is still settling in wave 2, and no `DEFAULT` declarations
  (parse error in v0.12.2 — writers set `created_at` and counters
  explicitly).

## Migrated modules

- **`invalidation.rs`** (dependency index + staleness): `(:Artifact
  {workspace, kind, id})-[:DEPENDS_ON {recorded_at}]->(:Artifact)`; the
  invalidation walk is one `[:DEPENDS_ON*1..]` MATCH; staleness is a `:Stale`
  marker label (with `stale_reason`/`stale_since` properties) so freshness
  scans are label filters and clearing is one `REMOVE` of label + properties.
  Public functions take `&GraphStore`; the few that still read SQL tables
  (`repo_chunks`, `gf_nodes`, `repo_projects`) take both connections.
- **`symbols.rs`** (reviewed memory anchored to code symbols): live symbols
  are `:SymbolHead` nodes (not `:Symbol`, so whole-table replacement never
  touches the code graph), the table's episode is a `:SymbolState` node,
  exact moves are `:SymbolMove` nodes, and pins are
  `(:Memory)-[:PINNED_TO {relpath, sha256, byte_start, byte_end, state, …}]->(:SymbolHead)`
  — a pending pin points at the repository's `stable_id: ""` anchor until
  resolved (the resolved links are the pin's `symbols` property, stringified
  JSON: the engine has no map/list-of-map properties). Read paths still
  threaded by SQL connections resolve the store with `store::graph_for`.

## Value-mapping conventions

- **JSON text columns — correction from the memory-pipeline node (verified
  on v0.12.2): a property value cannot be a map** (`InvalidPropertyType`;
  only scalars and lists of scalars store). `SET n += $map` works as a
  property-set *merge*, and **dotted property names** work everywhere —
  `SET n.`data.foo` = $v`, `WHERE n.`data.foo` = $v`, index declarations —
  so a JSON column with a known, fixed shape flattens into `data.foo`
  properties. Payloads with genuinely dynamic keys or arrays of objects
  (memory/claim/entity `data`, revision journals) stay **stringified JSON
  in a String property**, decoded at the module boundary exactly as
  `store::decode` did for SQL. `json_to_value`/`value_to_json` remain the
  parameter/result bridge (parameters and UNWIND rows may be maps; that is
  not property storage).
- **Ids are STRINGs.** A graph-type `UUID` declaration enforces *string*
  values and rejects `uuid()` results (spike E2 quirk), so ids are declared
  `IS STRING` and passed as strings — which is what graphicle's text ids
  already are.
- **Bytes are base64 strings** (`Value` has no byte-string variant; spike
  E4 verified the round-trip at +33% size): secrets ciphertext, commit
  object bytes, digests.
- Numbers: JSON integers → `Int`, other numbers → `Float`; floats JSON
  cannot carry (NaN/∞ ) read back as `null`. Temporal/point values read as
  their display strings; vectors read as number lists.
- **Vector properties are written with the `vector()` constructor**:
  `SET n.embedding = vector(row.vec, $dim, FLOAT64)` — the coordinate type
  is a bare keyword (a quoted `'FLOAT64'` parse-errors), and the index must
  exist first (`ensure_vector_indexes`).

## Engine limitation: no map property values (verified on v0.12.2)

cypherlite cannot store a **map** as a property value (`InvalidPropertyType`; only scalars and lists of scalars). `SET n += $map` works as a property-set *merge*. Consequence: JSON-object payloads (`data`/`metadata`/`attributes` columns) must stay stringified JSON in a `String` property or be flattened to scalar properties — never push a `Value::Map` into one property (so `json_to_value` is for lists/scalars only). Lists of scalars (e.g. ACLs) are fine and preferred for membership checks.

### Flattening JSON objects into dotted properties (verified on v0.12.2)

A map cannot be a property *value*, but property *names* may be dotted: `SET n.`+chr(96)+`attributes.foo`+chr(96)+` = $v`, `MATCH (n) WHERE n.`+chr(96)+`attributes.foo`+chr(96)+` = $v`, and `CREATE INDEX i FOR (n) ON (n.`+chr(96)+`attributes.foo`+chr(96)+`)` all work (backtick-quote the key). Preferred mapping for JSON-object columns (`attributes`, `data`, `metadata`): **flatten to dotted properties** so subfields stay queryable and indexable. Keep a single stringified-JSON String property only for genuinely dynamic/unbounded keys or arrays of objects.

## Migrated domains

- **Memory pipeline** (`memory-pipeline-cypher`): `repo_memories` →
  `:Memory` nodes (payload `data` stringified; `embedding` lives on the
  node, kept out of listings), `repo_memory_revisions` → `:MemoryRevision
  -[:REVISION_OF]->`, `am_claims`/`am_claim_events` → `:Claim` /
  `:ClaimEvent-[:FOR]->`, `am_entities` → `:Entity` with `am_entity_links`
  as `:Claim-[:HAS_SUBJECT|HAS_OBJECT]->(:Entity)`, `am_vectors` →
  `Vector`-typed `embedding` properties on `:Memory`/`:Chunk` (+ stub
  `:Chunk` nodes keyed by passage identity) searched through
  `SEARCH … VECTOR INDEX …` — the load-all-and-cosine pass over
  `am_vectors` in `query.rs` is gone. `Store` holds the `GraphStore`
  (`store.graph()`, `reviews.graph` beside `reviews.sqlite3`); the SQL
  tables remain in the schema files until the cleanup node drops them.
