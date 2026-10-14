# CypherLite engine spike — Graphicle `cypherlite-backend` validation

- Engine: neo4j-labs CypherLite **v0.12.2** (tag, clean worktree at `/tmp/cypherlite-v0.12.2`)
- Scratch crate: `/tmp/cl-spike` (`cypherlite` path dep), all experiments executed code with captured output
- Spike run: started 2026-10-14T22:55:10Z, finished 2026-10-14T23:13:20Z
- Machine: macOS (Darwin 25.6.0, arm64), release build (`--release`) for all measurements
- Note: the sibling checkout at `/Users/flm2dxqwm5/d/llm/cypherlite` is **13 commits ahead of v0.12.2 with uncommitted, non-compiling changes** (`cypherlite-executor/src/exec/bulk.rs` references a missing `mod run;`). All results below are from the clean `v0.12.2` tag in a separate git worktree.

API confirmation (matches the migration doc):
`cypherlite::Database::open(":memory:" | path) -> Database`; `db.connect() -> Connection`;
`conn.query_with_params(&str, BTreeMap<String, Value>) -> Result<QueryResult>` with
`QueryResult { columns: Vec<String>, rows: Vec<Vec<Value>>, stats: QueryStats, .. }`;
`conn.begin()/commit()/rollback()`, `db.checkpoint()`. All present as documented.

---

## E1 — Dynamic schema features: ALL WORK

Executed against `:memory:` (output verbatim):

```
CREATE (a:$(['Person','Admin'])) list literal      -> OK  labels=[Person, Admin]
CREATE (a:$($labels)) with param                   -> OK  labels=[Live, SymbolVersion]
CREATE (a)-[r:$($t)]-> dynamic rel type            -> OK  type(r)=CALLS
MERGE  (a)-[r:$($t)]-> dynamic rel type            -> OK  type(r)=CALLS
MATCH  (a)-[r:$($t)]-> dynamic rel type            -> OK  type(r)=CALLS
SET n:Live                                         -> OK
REMOVE n:Live                                      -> OK
SET n:$(add) REMOVE n:$(rem) (exprs)               -> OK
MATCH (n:T&Live&!Retracted)                        -> OK  (returned exactly the Live, non-Retracted node)
MATCH (n:Live&!Retracted)                          -> OK
MATCH (n:$(['T','Live']))                          -> OK
MATCH (n:T:Live&!Retracted)                        -> ERR InvalidLabelExpression:
                                                     "cannot mix colon-conjunction with label
                                                      expression symbols"
```

Verdict: dynamic labels via `:$(expr)` (incl. parameters) and dynamic rel types in CREATE / MERGE / MATCH
all work in v0.12.2. `SET n:Label` / `REMOVE n:Label` and dynamic `SET/REMOVE n:$(expr)` work.
Label expressions work with the pure `&`/! syntax (`n:T&Live&!Retracted`); mixing a second
colon-conjunction (`n:T:Live&!…`) is a parse error — trivially avoided by using `&`.

## E2 — GRAPH TYPE (Cypher 25): works, with concrete syntax corrections and one type quirk

The doc sketch §2.6 needs three syntax fixes for v0.12.2:

1. `=>` (or `IMPLIES`) after the identifying label is **mandatory** before a property block:
   `(:Workspace {…})` → parse error "Node type does not have an explicitly denoted identifying label".
2. Elements inside `{ }` are **comma-separated**.
3. Property declarations use `name IS <type>` (or `name :: <type>`); keys are **inline per property**
   (`id IS STRING IS KEY` / `IS UNIQUE`) or **CONSTRAINT elements** inside the body
   (`CONSTRAINT FOR (s:Symbol) REQUIRE (s.repo, s.sid) IS KEY`). The `KEY (…)` clause after an element
   and trailing `REQUIRE (a, b) IS KEY` forms both parse-error.
4. `NOT NULL` in a property declaration **parses** (`name IS STRING NOT NULL` accepted).
5. `DEFAULT <expr>` is **not supported** (parse error) — as expected, writers set defaults explicitly.

What ran (verbatim evidence):

```
CREATE GRAPH TYPE graphicle AS {
  (:Workspace => {id IS STRING IS KEY, name IS STRING NOT NULL}),
  (:Symbol => {repo IS STRING, sid IS STRING}),
  CONSTRAINT FOR (s:Symbol) REQUIRE (s.repo, s.sid) IS KEY
}                                                     -> OK
ALTER CURRENT GRAPH TYPE SET GRAPH TYPE g             -> OK   (promotion REQUIRED; declared != current)
```

Enforcement after promotion (e2b probe, all verified):

```
duplicate Workspace.id            -> ERR ConstraintValidationFailed: `constraint_key_Workspace_id` uniqueness
Workspace without name            -> ERR ConstraintValidationFailed: `constraint_propertyType_Workspace_name` property type
Workspace name: 42 (wrong type)   -> ERR (same constraint)
Symbol duplicate (repo,sid)       -> ERR ConstraintValidationFailed: `constraint_key_Symbol_repo_sid` uniqueness
Symbol missing sid                -> ERR ConstraintValidationFailed: ... requires properties
Symbol with undeclared property   -> OK  (open — undeclared props allowed)
Node with undeclared label        -> OK  (open)
```

Promotion derives real constraints (`SHOW CONSTRAINTS` lists `constraint_key_Workspace_id` NODE_KEY,
property-type constraints etc.). **Keys, mandatory (NOT NULL), and property types ARE enforced.**

Quirks found:
- **`UUID` declared type canonicalizes to `STRING`** in the stored spec and enforces String values —
  `CREATE (:Symbol {repository_id: uuid(), …})` is *rejected* ("property type") while string ids pass.
  Practical rule: declare id columns as STRING and store uuid strings (or keep ids as strings
  throughout — graphicle uses text ids anyway).
- Once a label is managed by the promoted graph type, independent constraints on it are rejected:
  `CREATE CONSTRAINT ws_name FOR (n:Workspace) REQUIRE n.name IS NOT NULL` →
  `IncompatibleGraphTypeDependence: dependent constraints are managed by the graph type`;
  duplicates → `ConstraintConflict` / `EquivalentSchemaRuleAlreadyExists`.
  So the graph type and hand-written constraints are **either/or per label**, not additive.

Fallback `CREATE CONSTRAINT` statements for the §2.6 entities: **all parse and enforce**
(verified `IS UNIQUE`, composite `IS NODE KEY`, `IS NOT NULL`; duplicate insert →
`ConstraintValidationFailed`). Exact list needed if the graph type is *not* used (or for labels not
declared in it):

```cypher
CREATE CONSTRAINT ws_id    FOR (n:Workspace)      REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT cred_key FOR (n:Credential)     REQUIRE n.capability_hash IS UNIQUE;
CREATE CONSTRAINT proj_key FOR (n:Project)        REQUIRE (n.workspace_id, n.project_key) IS NODE KEY;
CREATE CONSTRAINT obj_key  FOR (n:Object)         REQUIRE (n.repository_id, n.sha256) IS NODE KEY;
CREATE CONSTRAINT mem_id   FOR (n:Memory)         REQUIRE n.id IS UNIQUE;
CREATE CONSTRAINT job_key  FOR (n:Job)            REQUIRE (n.repository_id, n.kind, n.input_hash) IS NODE KEY;
CREATE CONSTRAINT sym_key  FOR (n:Symbol)         REQUIRE (n.repository_id, n.stable_id) IS NODE KEY;
CREATE CONSTRAINT ep_key   FOR (n:Episode)        REQUIRE (n.repository_id, n.sequence) IS NODE KEY;
CREATE CONSTRAINT run_key  FOR (n:Run)            REQUIRE (n.repository_id, n.external_key) IS NODE KEY;
CREATE CONSTRAINT del_key  FOR (n:DeletionReceipt) REQUIRE (n.workspace_id, n.sequence) IS NODE KEY;
```
(`IS NODE KEY` implies uniqueness + presence; verified: duplicate composite insert fails, missing
property fails with "requires properties".) Recommendation: use the graph type (single DDL artifact,
enforced), declare ids as STRING, and keep per-label fallback constraints only for labels not in the
graph type.

## E3 — Edge close strategy: both work; type-flip 3x slower on close, faster on live reads

10k rels created, then closed in ONE statement each (release, :memory:):

```
type-flip:  MATCH (a)-[r:CALLS]->(b) WITH a, b, r, r.key AS k DELETE r
            CREATE (a)-[:CALLS_HIST {key:k, valid_from_episode:'e0', valid_to_episode:'e1'}]->(b)
            -> 88.9 ms for 10k closes (~112k closes/s)   live=0 hist=10000 afterwards
property:   MATCH ()-[r:CALLS]->() SET r.valid_to_episode = $x
            -> 28.2 ms for 10k closes (~355k closes/s)
live read (count over :CALLS):            6.2 ms (type-flip graph, clean partition)
live read (WHERE r.valid_to_episode IS NULL): 13.7 ms (property graph)
```

Gotcha: `DELETE r` then reading `r.key` in the same statement fails with
`DeletedEntityAccess: cannot access properties of a deleted relationship` — snapshot properties in a
`WITH` **before** the DELETE (as above).

Recommendation: **type-flip** as designed in the migration doc. The 3x close cost (~9µs/edge) is
immaterial at graphicle write volumes, and it keeps live traversals on clean rel-type partitions with
no per-edge property filter — the dominant read path. Property fallback stays viable if profiling
later shows close-heavy episodes matter.

## E4 — Bytes: base64 string round-trip works

1 MiB pseudo-random bytes → base64 (1,398,104 chars, +33.3%) → stored as String property → read back
→ byte-identical after decode (`roundtrip_equal=true`). Store 2.3 ms, read 0.22 ms (:memory:).
No Bytes variant in `Value` confirmed (enum: Null/Bool/Int/Float/String/List/Map/Node/Relationship/
Path/Temporal/Point/Uuid/Vector). Interim strategy confirmed: base64 strings.

## E5 — Cargo features: `default-features = false` drops parquet AND arrow

```
# cypherlite = { path = … }                       (default = ["parquet"])
$ cargo tree -e normal -i parquet
parquet v54.3.1
└── cypherlite-storage v0.12.2
    └── cypherlite v0.12.2
        └── cl-spike v0.1.0
$ cargo tree -e normal -i arrow     -> arrow v54.3.1 (same chain)

# cypherlite = { path = …, default-features = false }
$ cargo tree -e normal -i parquet
error: package ID specification `parquet` did not match any packages
$ cargo tree -e normal -i arrow
error: package ID specification `arrow` did not match any packages
```

The flag is **not** a no-op on the v0.12.2 tree: the whole arrow/parquet subtree disappears.
The facade compiles and all experiments (incl. vector/fulltext SEARCH, directory store) pass with
`default-features = false` — snapshots then use the JSON codec.

## E6 — Batch / transactional writes

```
UNWIND $rows AS row MERGE (n:N {id: row.id}) SET n.name = row.name     -- 10k rows, 1 stmt
  -> 62 ms (161k rows/s), stats nodes_created=10000 properties_set=20000
CALL { UNWIND $rows AS row MERGE … } IN TRANSACTIONS OF 1000 ROWS      -- 10k rows
  -> OK, but 18.4 s (!!) vs 62 ms plain — ~300x slower on :memory:.
     Works functionally (20k nodes total afterwards) but per-batch commit is expensive here;
     use it only where resumability is worth the cost, with larger batches / on-disk stores re-measured.
UNIQUE violation: CREATE (:N {id:1}) with constraint -> semantic error:
  ConstraintValidationFailed: `uniq_n` uniqueness       (surfaces as Err, connection stays usable)
begin(); CREATE; rollback(); -> row absent   ✓
begin(); CREATE; commit();   -> row present  ✓
```

## E7 — SEARCH: vector + fulltext both work; fulltext auto-updates on writes

```cypher
CREATE VECTOR INDEX chunk_embed FOR (n:Chunk) ON n.embedding
  OPTIONS {indexConfig: {`vector.dimensions`: 4, `vector.similarity_function`: 'cosine'}}   -> OK
CREATE FULLTEXT INDEX chunk_text FOR (n:Chunk) ON EACH [n.index_text]                     -> OK
MATCH (n:Chunk) SEARCH n IN (VECTOR INDEX chunk_embed FOR $qv LIMIT 2) SCORE AS sc
  RETURN n.id, sc              -> OK  [(1, 1.0), (3, 0.997)]   ($qv List-of-Float param works)
MATCH (n:Chunk) SEARCH n IN (FULLTEXT INDEX chunk_text FOR 'rust' LIMIT 10) RETURN n.id
                               -> OK  [1, 3]
-- after CREATE of a new 'rust' chunk: SEARCH returns 3 rows (incl. id 4)  ✓ auto-update
-- after SET n.index_text = 'oxidized …': SEARCH 'oxidized' finds id 2     ✓ auto-update
```

No triggers needed; the fulltext index maintains itself on CREATE and SET. Vector SEARCH supports
parameter query vectors and `SCORE AS`.

## E8 — Load tests (release build)

Synthetic graph: 100k `:Symbol` nodes (id/stable_id/kind/qualified_name/relpath), 500k rels
(300k CALLS + 100k CONTAINS + 100k IMPORTS, random endpoints, dynamic `-[r:$($t)]->` creation).
Range index on `Symbol.id`. Raw output:

```
(a) bulk load, UNWIND batch sizes (:memory: and directory store):
[mem] nodes 100k, batch 1k    -> 203,325 nodes/s   (491.8 ms)
[mem] nodes 100k, batch 10k   -> 177,977 nodes/s   (561.9 ms)
[mem] nodes 100k, batch 100k  -> 168,323 nodes/s   (594.1 ms)
[mem] rels CALLS 300k, batch 1k    -> 88,387 rels/s
[mem] rels CONTAINS 100k, batch 1k -> 90,489 rels/s
[mem] rels IMPORTS 100k, batch 1k  -> 91,079 rels/s
[dir] nodes 100k, batch 10k   -> 187,358 nodes/s   (WAL+durable store, no meaningful penalty)
[dir] rels CALLS 300k, batch 10k   -> 55,155 rels/s
[dir] rels CONTAINS 100k, batch 10k -> 70,923 rels/s
[dir] rels IMPORTS 100k, batch 10k  -> 71,409 rels/s

(b) MERGE-heavy projection-style writes (:memory:):
run1 (mostly inserts): 50k nodes in 906.8 ms -> 55,140/s ; 100k rels in 2.72 s -> 36,828/s
run2 (all updates):    50k nodes in 892.7 ms -> 56,009/s ; 100k rels in 2.57 s -> 38,892/s

(c) live-graph read latency, 200 random start nodes (:memory: / dir):
[mem] 2-hop CALLS: p50=0.06 ms  p95=0.10 ms  mean=0.06 ms
[mem] 3-hop CALLS: p50=0.16 ms  p95=0.32 ms  mean=0.17 ms
[dir] 2-hop CALLS: p50=0.04 ms  p95=0.07 ms
[dir] 3-hop CALLS: p50=0.08 ms  p95=0.15 ms
```

Takeaways:
- Batch size barely matters for node CREATE (1k–100k within 20%); use ~10k as a sane default.
- Rel CREATE with endpoint MATCH runs ~90k rels/s (mem) / ~55–71k (dir) — a 500k-edge episode
  loads in ~5–9 s.
- MERGE upserts: ~55k nodes/s, ~37–39k rels/s; second all-update run is the same speed (no
  degradation on update-heavy passes) — projection re-runs are fine.
- Traversal latency is excellent: sub-millisecond p95 for 3-hop at 100k/500k scale, both stores.

---

## DECISIONS

- **rel_type_strategy: `dynamic`** — `:$(expr)` works for labels and rel types in CREATE/MERGE/MATCH;
  no enum or `:REL {relation}` fallback needed. Extractor relation strings map directly to rel types.
- **bytes_strategy: `base64-string`** — round-trip verified, +33% size, ~ms latency at 1 MiB.
- **feature_flags: `default-features = false`** — verified to drop arrow/parquet entirely on v0.12.2
  with full functionality (JSON snapshot codec). If parquet snapshots are wanted later, flip to
  `default` — both verified working.
- **edge_close strategy: `type-flip`** (DELETE live rel, CREATE `*_HIST` with validity props,
  properties snapshotted via WITH before DELETE). 3x slower per close than a property flag
  (~9 µs vs ~3 µs per edge) but keeps live reads on pure type partitions (2x faster live scans,
  and the advantage grows with history depth since property-filtered scans pay per historical edge).
- **Blockers / caveats discovered:**
  1. The `../cypherlite` checkout is dirty and does not compile — use tag `v0.12.2` (worktree or pin).
  2. Graph type: UUID property type behaves as STRING (declared `IS UUID` enforces string values;
     `uuid()` values are rejected). Declare ids STRING, pass strings.
  3. Graph type must be **promoted** (`ALTER CURRENT GRAPH TYPE SET GRAPH TYPE <name>`) before it
     enforces anything; `CREATE GRAPH TYPE` alone is inert.
  4. Graph type and hand-written constraints conflict per label (`IncompatibleGraphTypeDependence` /
     `EquivalentSchemaRuleAlreadyExists`) — pick one mechanism per label.
  5. No `DEFAULT` in graph type property declarations (parse error) — writers set defaults.
  6. `DELETE r` + read `r.prop` in one statement → DeletedEntityAccess; snapshot props in WITH first.
  7. `CALL {} IN TRANSACTIONS OF n ROWS` is ~300x slower than a plain UNWIND on :memory: for MERGE
     workloads (18.4 s vs 62 ms per 10k rows) — measure before adopting for the projection load path;
     plain UNWIND batches inside an explicit begin/commit may be the better bulk path.
