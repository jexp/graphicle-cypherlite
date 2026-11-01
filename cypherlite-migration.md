# CypherLite migration plan — replacing SQLite in `crates/graphicle`

_2026-10-15. Companions: `findings.md` (architecture), `sqlite-usage.md` (what exists today)._

## 0. Engine choice

**neo4j-labs CypherLite v0.12.2**, embedded as Rust **engine crates only** — the `cypherlite` facade (`cypherlite/cypherlite`, "Embedded in-memory OpenCypher/Cypher25 graph database") plus its internal engine deps (`core`, `storage`, `parser`, `planner`, `executor`, …). The `surfaces/*` crates (CLI, Bolt/HTTP servers, Python/WASM drivers, MCP) are **not** used.

- Local sibling checkout at `../cypherlite` (v0.12.2, clean) → path dependency:
  `cypherlite = { path = "../cypherlite/cypherlite/cypherlite" }`
  (CI/release will need a git dep pinned to tag `v0.12.2` or vendoring — the crates are **not on crates.io**. Beware: crates.io `cypherlite-*` 2.0.1 crates are an unrelated Epsilondelta-ai project — no constraints, no CALL {}, no vector/fulltext, rollback is a stub. Do not use.)
- API shape is rusqlite-like, which keeps the `Store` façade shape:
  `Database::open(uri)` (`:memory:` / directory / `.cl` file) → `db.connect() -> Connection` → `conn.query_with_params(cypher, BTreeMap<String, Value>) -> QueryResult { columns, rows: Vec<Vec<Value>>, … }`. Explicit `begin()/commit()/rollback()` on Connection; drop = implicit rollback.
- `Database` is `Send + Sync` (Arc-shared); one `Connection` per thread (`&mut self`, Send but !Sync, spawns a 64 MiB-stack worker thread per connection). Fits the existing `Arc<Mutex>`-ish server state.
- Confirmed available: full openCypher TCK (3892/3892 pinned), MERGE, `UNWIND $rows AS row`, `CALL {}`, `COUNT/EXISTS/COLLECT {}` subqueries **incl. `CALL {} IN TRANSACTIONS`**, FOREACH, DETACH DELETE, `SET +=`, variable-length/quantified paths, shortestPath; UNIQUE / NOT NULL / NODE KEY / property-type constraints; range, **VECTOR**, **FULLTEXT**, point, text, lookup indexes; `SEARCH n IN (VECTOR|FULLTEXT INDEX …)` clause; dynamic properties on nodes **and** relationships (maps, lists, temporal, vector, uuid), patter/map/list comprehensions.
- see https://neo4j.com/docs/cypher-manual/current/cheat-sheet/
- Storage: `:memory:` (ephemeral — use for tests and transient processing), directory store (WAL + snapshots), single-file `.cl`. Durability modes Full/EverySecond/Snapshot.

## 1. Strategy: rip and replace behind the `Store` façade

Decision (agreed in chat): **no storage-abstraction trait**. A trait would either mirror SQL (forcing graph ops into a row-shaped API — pointless) or pre-design the graph API we can only learn by writing it. There is only ever one backend. Instead:

1. Keep `Store` as the entry point (`open`/`transaction`/`read`/`authorized*`), swap its internals from rusqlite `Connection` to `cypherlite::Database` + per-call `Connection`.
2. Rewrite each module's data-access body from SQL to Cypher, module by module. Module function signatures (serde_json::Value in/out) stay — callers and the `graphicle-memory` JSON boundary don't change.
3. `tests/it/*` is the parity net; add a SQLite→CypherLite export/import migration tool for existing `reviews.sqlite3` installs.
4. Deliberately dropped: FTS5 `repo_search` (write-only, replaced by a real FULLTEXT index), all triggers (invalidation moves into the write path), `sqlite_master` enumeration (label-scoped deletes), `ensure_column` (schemaless properties), rusqlite backup API (→ engine checkpoint/snapshot/clone).

## 2. Graph model

Principles: every FK becomes a **relationship**; every join table becomes a **relationship with properties** (or a rel between first-class nodes); table names become **labels**; JSON-text columns become **map/list properties**; validity windows become **properties on versioned nodes/relationships**.

### 2.1 Ownership spine

```
(:Workspace {id, capability_hash, name, created_at, version})
(:Credential {id, capability_hash, role, label, created_at, expires_at, revoked_at})
  -[:IN_WORKSPACE]->(:Workspace)
(:Source {id, name, role, content, sha256, created_at, version}) -[:IN_WORKSPACE]->(:Workspace)
(:Workspace)-[:SOURCE_HEAD {name, role}]->(:Source)              -- replaces source_heads
(:Review {id, source_version, data, created_at}) -[:IN_WORKSPACE]->(:Workspace)
(:Review)-[:HAS_DECISION]->(:Decision {change_id, status, note, updated_at})
(:Project {id, workspace_id, project_key, name, snapshot, generation, metadata, …})
  -[:IN_WORKSPACE]->(:Workspace)                                 -- repo_projects
```

### 2.2 Content store (CAS)

```
(:Project)-[:HAS_OBJECT]->(:Object {sha256, content, bytes})     -- repo_objects, MERGE-scoped
(:File {path}) -[:HEAD]->(:Object)                               -- repo_heads
(:File)-[:IN_PROJECT]->(:Project)
(:File)-[:PARSED_AS]->(:Parse {sha256, revision, data})          -- repo_parses
(:Chunk {line, end_line, index_text}) -[:PART_OF]->(:Object)     -- repo_chunks
(:Snapshot {sha256, manifest, metadata, created_at}) -[:OF]->(:Project)
(:Sync {id, state, expected_snapshot, snapshot, manifest, …}) -[:OF]->(:Project)
(:Event {seq, kind, data, created_at}) -[:OF]->(:Project)        -- append-only log
(:Project)-[:HAS_CHUNK…] -- scoping rels where queries need project-bounded scans
```

### 2.3 Sessions & memory

```
(:Run {id, external_key, session_key, title, state, …}) -[:OF]->(:Project)
(:ToolEvent {event_key, seq, data, created_at}) -[:OF]->(:Run)
(:Memory {id, kind, revision, state, title, body, data, …}) -[:OF]->(:Project)
(:MemoryRevision {revision, data, created_at}) -[:REVISION_OF]->(:Memory)
(:EvidenceLink {kind, resource_id, follow_head, created_at}) -[:OF]->(:Project)
(:Claim {id, state, data, source_state, …}) -[:OF]->(:Project)
(:Claim)-[:ABOUT_MEMORY]->(:Memory)
(:Entity {id, name, kind, data}) -[:OF]->(:Project)
(:Claim)-[:HAS_SUBJECT]->(:Entity)                               -- am_entity_links triple
(:Claim)-[:HAS_OBJECT]->(:Entity)                                --   as two typed rels
(:Entity)-[:ALIAS_OF {proposal_id, score, state, …}]->(:Entity)  -- am_/gf_alias_proposals
```

### 2.4 Code graph — bi-temporal projection (the centerpiece)

Stable identity nodes + versioned relationships, validity as properties:

```
(:Episode {episode, sequence, node_count, edge_count, attributes, created_at}) -[:OF]->(:Project)
(:Episode)-[:FOLLOWS]->(:Episode)                                -- parent_episode DAG
(:Symbol {repository_id, node_id|stable_id, kind, qualified_name, relpath, …})
(:Symbol)-[:CALLS {valid_from_episode, valid_to_episode, key, …}] (etc.)->(:Symbol)
```

- Extractor relation strings become **relationship types**: `CALLS`, `CONTAINS`, `IMPORTS`, `IMPORTS_FROM`, `INHERITS`, `USES`, … (open item: verify dynamic rel-type creation, else a bounded enum, else `:REL {relation}` fallback — see §6).
- **Active/inactive via labels and types, not sentinel properties.** Versioned node state: `(:Symbol)-[:HAS_VERSION]->(:SymbolVersion:Live)` — at most one `:Live` version per symbol; closing a version is `REMOVE v:Live` + `SET v2:Live` (cypherlite has dynamic label ops; spike verifies). Live reads are label partitions (`MATCH … (v:Live)`), the cheapest filter available, replacing the partial index `WHERE valid_to_episode IS NULL`. Relationships can't carry labels (a rel has one immutable type), so edges use a **type-flip**: live `(:a)-[:CALLS]->(:b)`; on close the rel is recreated as `:CALLS_HIST {valid_from_episode, valid_to_episode, …}`. Live traversals never scan history; history queries use the `*_HIST` types explicitly. Fallback if the spike shows recreate-on-close is too costly: keep `valid_to_episode` as a rel property and filter `IS NULL`.
- Bi-temporal versions: episode validity windows live on the `*_HIST` relationships (and `SymbolVersion` nodes); parallel edges between the same pair are native (multigraph by rel type + `key` property).
- Provenance: edge properties `relpath, source_location, derivation` (replaces `gf_edge_sources` join table).
- Contradictions: `(:Contradiction {episode, relation, subject, old_target, new_target, recorded_at}) -[:OF]->(:Project)`.
- Cross-repo: `(:Symbol)-[:CROSS_CALL {relation, confidence, derivation, module_path, symbol, line, version_…}]->(:Symbol)` with `source_repository_id/target_repository_id` scoping props; `(:UnresolvedCross {…}) -[:OF]->(:Project)`.
- Symbols↔memory: `(:Memory)-[:PINNED_TO {relpath, sha256, byte_start, byte_end, state, episode}]->(:Symbol)` (replaces gf_symbol_pins); `gf_symbol_heads/state/moves` fold into `:Symbol` properties + `(:SymbolMove)-[:OF]->(:Project)`.
- Timeline/commits: `(:Assertion {branch, episode, valid_us, recorded_us, origin}) -[:ON_TIMELINE]->(:Project)`; `(:Commit {oid, tree_oid, …})-[:PARENT {position}]->(:Commit)` (replaces gf_commit_parents); aliases/hyperedges as versioned rels/nodes with `valid_us/recorded_us` props.
- Coverage: `(:Test {test_id})-[:COVERS {episode, line_start, line_end}]->(:File)`.

### 2.5 Derived data, jobs, account

- Invalidation: `(:Artifact {kind, id})-[:DEPENDS_ON]->(:Source {kind, id})` (workspace-scoped); staleness as `(:Artifact {stale_since, stale_reason})` props or `(:Stale)` marker label. **The dependency walk becomes a variable-length traversal** instead of covering-index scans.
- Doc links: `(:ConnectorDoc)-[:LINKS_TO {rung, confidence, …}]->(:Symbol)` (+ checkpoint nodes).
- Communities: `(:Community {community_id, episode, level, …})-[:HAS_MEMBER]->(:Symbol)`, `(:Report)-[:ABOUT]->(:Community)`, stable-id rels `(:Community)-[:CONTINUES {overlap}]->(:Community)`.
- Observations/hints: `(:Observation)`, `(:MentalModel)`, `(:Hint {scope, target, weight, state})`, `(:Served)`, `(:Outcome)`, `(:HeldOut)` — all `-[:OF]->(:Project)`; mental model `-[:GROUNDED_IN]->(:Observation)`.
- Jobs: `(:Job {id, kind, input_hash, state, priority, attempt, not_before, lease_until, lease_token, …})-[:OF]->(:Project)`; `(:ModelCall)-[:FOR]->(:Job)`; `(:Reservation)-[:FOR]->(:Job)`. Queue query = `MATCH (j:Job) WHERE j.state='ready' AND j.not_before <= $now ORDER BY j.priority DESC … SET` (CAS lease via property predicate in the same statement).
- Connectors: `(:Connector)-[:OF]->(:Project)`, `(:ConnectorDoc)-[:FROM]->(:Connector)` with `acl` as **list property** (replaces `json_each` joins: permission checks become list membership / `(:Principal)-[:CAN_ACCESS]->(:ConnectorDoc)` rels where that pays off).
- Account: `(:Secret)`, `(:OAuthAccount)-[:USES]->(:Secret)`, `(:Usage {day, requests, errors, elapsed_ms})-[:OF]->(:Workspace)`, `(:DeletionReceipt {sequence, digest, previous})-[:NEXT]->(:DeletionReceipt)` (hash chain as a linked list).

### 2.6 Graph type (schema) — not hand-written constraints

The schema is declared as a **graph type** (Cypher 25 `GRAPH TYPE`), not as a pile of individual `CREATE CONSTRAINT` statements: node/edge types with typed, mandatory properties and keys in one definition. **Spike-verified syntax on v0.12.2** (see `docs/spike-cypherlite.md` E2): `=>` after the identifying label, properties as `name IS TYPE`, keys inline (`id IS STRING IS KEY`) or as `CONSTRAINT` elements, elements comma-separated, `NOT NULL` accepted, `DEFAULT` **parse-errors** (defaults arrive in a later version — writers set `created_at`/counters explicitly until then). A graph type must be **promoted** (`ALTER CURRENT GRAPH TYPE SET GRAPH TYPE graphicle`) before it enforces; after promotion keys/mandatory/property-types are genuinely enforced. Quirks: declared `UUID` behaves as `STRING` (engine `uuid()` values are rejected — **declare ids as STRING**), and a graph-type-managed label rejects additional hand-written constraints (either/or per label). Sketch:

```cypher
CREATE GRAPH TYPE graphicle {
  (w:Workspace => {id IS STRING IS KEY, capability_hash IS STRING NOT NULL,
                   name IS STRING NOT NULL, created_at IS STRING NOT NULL, version IS INTEGER}),
  (c:Credential => {id IS STRING, capability_hash IS STRING IS KEY, role IS STRING,
                    created_at IS STRING, expires_at IS STRING, revoked_at IS STRING}),
  (p:Project => {id IS STRING, workspace_id IS STRING NOT NULL, project_key IS STRING NOT NULL,
                 name IS STRING, snapshot IS STRING, generation IS INTEGER, metadata IS STRING},
   CONSTRAINT {p.workspace_id, p.project_key} IS NODE KEY),
  (o:Object => {repository_id IS STRING NOT NULL, sha256 IS STRING NOT NULL,
                content IS STRING, bytes IS INTEGER},
   CONSTRAINT {o.repository_id, o.sha256} IS NODE KEY),
  (s:Symbol => {repository_id IS STRING NOT NULL, stable_id IS STRING NOT NULL, kind IS STRING,
                qualified_name IS STRING, relpath IS STRING},
   CONSTRAINT {s.repository_id, s.stable_id} IS NODE KEY),
  (v:SymbolVersion => {…}),                    -- :Live label marks the current one
  (e:Episode => {repository_id IS STRING NOT NULL, episode IS STRING, sequence IS INTEGER},
   CONSTRAINT {e.repository_id, e.sequence} IS NODE KEY),
  (j:Job => {id IS STRING, repository_id IS STRING NOT NULL, kind IS STRING NOT NULL,
             input_hash IS STRING NOT NULL, state IS STRING NOT NULL, priority IS INTEGER},
   CONSTRAINT {j.repository_id, j.kind, j.input_hash} IS NODE KEY),
  (m:Memory => {id IS STRING IS KEY, kind IS STRING, state IS STRING, title IS STRING, body IS STRING}),
  (r:Run => {id IS STRING, repository_id IS STRING NOT NULL, external_key IS STRING NOT NULL},
   CONSTRAINT {r.repository_id, r.external_key} IS NODE KEY),
  (d:DeletionReceipt => {workspace_id IS STRING NOT NULL, sequence IS INTEGER, digest IS STRING},
   CONSTRAINT {d.workspace_id, d.sequence} IS NODE KEY),
  -- edge types with typed properties:
  (:Symbol)-[:CALLS {key IS STRING, relpath IS STRING, source_location IS STRING}]->(:Symbol),
  (:Symbol)-[:CALLS_HIST {valid_from_episode IS STRING, valid_to_episode IS STRING}]->(:Symbol),
  (:Symbol)-[:HAS_VERSION]->(:SymbolVersion),
  (:File)-[:HEAD]->(:Object)
};
ALTER CURRENT GRAPH TYPE SET GRAPH TYPE graphicle;
```

- **Spike-verified fallback**: all ten individual `CREATE CONSTRAINT … REQUIRE … IS UNIQUE / IS NODE KEY` statements work on v0.12.2 for labels the graph type does not manage.

### 2.7 Indexes

```
-- range: every -[:OF]-> scoping + state/queue lookups
CREATE INDEX job_ready   FOR (j:Job)    ON (j.state);        -- + not_before, priority
CREATE INDEX sym_file    FOR (s:Symbol) ON (s.relpath);
CREATE INDEX chunk_path  FOR (c:Chunk)  ON (c.path);         -- or via PART_OF traversal
CREATE INDEX mem_lookup  FOR (m:Memory) ON (m.kind);
-- fulltext (replaces dead FTS5, and makes it actually used):
CREATE FULLTEXT INDEX chunk_text FOR (c:Chunk)  ON EACH [c.index_text];
CREATE FULLTEXT INDEX mem_text   FOR (m:Memory) ON EACH [m.title, m.body];
CREATE FULLTEXT INDEX doc_text   FOR (d:ConnectorDoc) ON EACH [d.content];
-- vector (replaces load-all-and-cosine-in-Rust over am_vectors):
CREATE VECTOR INDEX chunk_embed FOR (c:Chunk)  ON (c.embedding) WITH [c.model_key];
CREATE VECTOR INDEX mem_embed   FOR (m:Memory) ON (m.embedding) WITH [m.model_key];
CREATE VECTOR INDEX ent_embed   FOR (e:Entity) ON (e.embedding) WITH [e.model_key];
```

Embeddings move from `am_vectors` JSON text onto `Vector`-typed properties of the node they describe (Chunk/Memory/Entity); similarity search becomes `SEARCH n IN (VECTOR INDEX chunk_embed FOR $qv LIMIT $k)`.

## 3. Read patterns (no N+1, no mega-joins)

| Today (SQL) | Cypher replacement |
|---|---|
| Live graph load: gf_nodes ⋈ gf_edges ⋈ gf_edge_sources ⋈ gf_episode_labels filtered by episode | `MATCH (s:Symbol {repository_id:$r})-[:HAS_VERSION]->(v:Live) MATCH (s)-[r:CALLS]->(t) …` — label partition + live rel types, one traversal (chained MATCHes, no comma patterns) |
| sync_prepare/sync_commit per-file `SELECT EXISTS` loops | `UNWIND $paths AS p MATCH … RETURN` — one round trip |
| ACL: 5-table join + `json_each` | `MATCH (c:Credential)-[:CAN_ACCESS]->(d)` or list-membership predicate on `d.acl` |
| Bi-temporal window functions (`row_number() OVER …`) | `MATCH … WITH … ORDER BY valid_us DESC, recorded_us DESC` + `CALL {}` per-group `LIMIT 1`, or `collect()` + list head |
| Invalidation walk over covering index | `MATCH (a:Artifact)-[:DEPENDS_ON*1..]->(src)` variable-length |
| In-Rust graph walks (no recursive CTE existed) | Native variable-length `MATCH`, `SHORTEST 1` |
| All-vectors load + Rust cosine | `SEARCH n IN (VECTOR INDEX …)` |
| FTS5 (unused) | `SEARCH n IN (FULLTEXT INDEX chunk_text FOR $q LIMIT $k)` |

## 4. Write patterns (batched, transactional)

- Bulk upsert (chunks, parses, objects, projection nodes/edges): one statement per entity kind:
  `UNWIND $rows AS row MERGE (o:Object {repository_id: row.repo, sha256: row.sha}) SET o += row.props` with `$rows` a `Value::List` of maps.
- Bulk projection load (biggest write path): plain **`UNWIND $rows` batches inside explicit `begin/commit`** — spike E6 measured `CALL {} IN TRANSACTIONS` ~300× slower on `:memory:` in v0.12.2 (18.4s per 10k rows), so reserve `IN TRANSACTIONS` for cases that genuinely need per-batch commit; chunking + explicit transactions gives the same WAL bounding without the overhead (replaces the savepoint-dance in `graph_delta.rs`).
- Spike load-test numbers (release, 100k nodes / 500k rels): bulk load 168–203k nodes/s and ~90k rels/s (`:memory:`), 55–71k rels/s on a directory store; MERGE upserts ~55k nodes/s, ~38k rels/s; 2-hop traversal p50 0.06ms, 3-hop p50 0.16ms. Batch size 1k vs 100k barely matters.
- Head swap (delete+insert repo_heads): `MATCH (f:File …) UNWIND $rows … MERGE … SET` in one `begin/commit`.
- Job leasing: single-statement CAS — `MATCH (j:Job {id:$id}) WHERE j.lease_token = $old SET j.lease_token=$new, j.lease_until=$t`.
- Multi-entity writes (sync commit touching objects/heads/parses/events): one explicit `Connection::begin()` … `commit()`; rollback on error (drop = implicit rollback).
- Write-write conflicts (optimistic commit): classified-retry wrapper in `Store::transaction`.
- Ephemeral processing (graph_materialize staging, expansion/dedup scratch, tests): `Database::open(":memory:")` — replaces test-only `open_in_memory` and gives a clean scratch graph for transient computation.

## 5. Migration of existing data

One-time `graphicle migrate-store` tool (per §7 decision): **SQL export to CSV from `reviews.sqlite3`, then import into CypherLite via `LOAD CSV` scripts** — one script per domain, batched `UNWIND` upserts where relationships need MERGE-matching. Covers every domain, including git history (`gf_commits`/`gf_commit_parents` → `(:Commit)-[:PARENT]->(:Commit)`, `gf_timeline`) and identity/account data (credentials, OAuth accounts, provider identities/emails). Forward-only; the original sqlite file is left untouched as the backup. No rusqlite dependency in the runtime at all — the exporter is a standalone script/tool.

## 6. Issues / risks that could block or shape adoption

1. **Not on crates.io** — path dep to `../cypherlite` works locally; CI/release packaging needs a git dep pinned to `v0.12.2` or vendoring. Also a **name collision**: crates.io `cypherlite-*` 2.0.1 (Epsilondelta-ai) is a different, much weaker engine — must not be picked up accidentally.
DECISION: IS OK
   - ⚠️ **Spike finding**: the sibling checkout `../cypherlite` is currently 13 commits past v0.12.2 with uncommitted, **non-compiling** changes (`cypherlite-executor/src/exec/bulk.rs` references a missing `mod run;`). The spike ran against a clean worktree of tag `v0.12.2`. Clean up the checkout (or point the path dep at a pinned worktree/git dep) before the facade node starts.
2. **Experimental, pre-1.0** (Neo4j Labs disclaimer): forward-only on-disk format migration between releases; pin versions and re-verify at upgrade.
DECISION: IS OK

3. **No Bytes/Blob value type** — `Value` has no byte-string variant. Affected: `am_secrets` ciphertext/nonce, `gf_commits` object, `am_original_documents` bytes, `gf_projection_groups` digest. Options: base64 strings (simple, +33% size) or sidecar files with hash references (better for large blobs — `repo_objects.content` is TEXT today so code content is fine as String).
DECISION: it should have byte[] until then we can use string
   - ⚠️ **Wave-2 finding (verified)**: cypherlite also can't store a **map** as a property value (only scalars and lists of scalars; `SET n += $map` merges). JSON-object columns (`data`, `metadata`, `attributes`) are **flattened into dotted properties** — property *names* may be dotted (`SET n.\`attributes.foo\` = $v`, indexed via `CREATE INDEX … ON (n.\`attributes.foo\`)`), so subfields stay queryable/indexable; a stringified-JSON String property remains the fallback for genuinely dynamic keys or arrays of objects. Lists of scalars (ACLs) are plain list properties. Recorded in `docs/store-api.md`.

4. **Single-level transactions, no savepoints** — `graph_delta.rs::ProjectionWrite` (drop/reinstall triggers in a SAVEPOINT with rollback-on-Drop) must be redesigned: explicit txn + in-Rust compensation, or `CALL {} IN TRANSACTIONS` batching.
DECISION: IS OK

5. **Optimistic concurrency** — write-write conflict at commit (element-id level, no write-skew detection); constraints checked per statement, not across concurrent txns. Graphicle is single-writer via the job queue, so exposure is small, but `Store::transaction` needs a retry loop.

DECISION: IS OK

6. **Per-Connection 64 MiB worker thread** — don't create connections per request; pool a small fixed set (or one per worker thread).

DECISION: IS OK

7. **Dynamic relationship types and label flips** — extractors emit relation strings (`calls`, `contains`, …) and the active/inactive design relies on `SET/REMOVE :Live` label flips plus rel-type-flip (`CALLS` → `CALLS_HIST` delete+recreate) on edge close. Need to verify in cypherlite v0.12.2: computed rel types in CREATE/MERGE, dynamic label ops, and the cost of recreate-on-close; fallbacks are a bounded type enum + `:REL {relation}` and a `valid_to_episode` property filter. *(First spike task.)*

DECISION: IS OK, we have dynamic labels with :$(labels) and same for rel-types and properties, see the cheat sheet

8. **Triggers don't exist** — projection-group invalidation and FTS sync triggers move into the write path explicitly (also an opportunity: the FTS table was write-only dead weight).

FTS is automatically updated

9. **AUTOINCREMENT ordering** — `repo_events`/`am_deletion_receipts` rely on monotonic rowids; replace with a counter node (`MERGE (c:Counter {name}) SET c.value = c.value + 1` in the same txn) or `(created_at, uuid)` ordering.

DECISION: IS OK, there is an uuid and timestamp function, graph type will get `default()` in the next version

10. **Backup/restore** — rusqlite backup API (maintenance.rs live backup, local.rs restore, `scrub_backup` hook) has no 1:1 equivalent; redesign on engine checkpoint/snapshot (`snapshot_data_bytes`, `.cl` bundle copy, `clone`). Extension trait signatures (`migrate(&Connection)`, `scrub_backup`) change — part of the rewrite.

DECISION: IS OK, the file can be copied when the db is closed, the cypherlite cli also has tools for it

11. **Dependency weight** — default `parquet` feature pulls arrow/parquet; consider `default-features = false` (JSON snapshot codec) if snapshot size isn't critical — verify the flag works on the v0.12.2 tree (docs hint it may be a no-op before the next release).

DECISION: parquet is for store files, we could use json mode, arrow is afaik used for pandas only, but for now both are ok

12. **Single-process lock** — fine for current architecture (one server per store); would block any future multi-process tooling reading the store live (mitigation: read-only opens can coexist with the writer).

DECISION: IS OK


## 7. Execution outline (feeds /gbuild:plan)

1. **Spike**: embed `cypherlite` from `../cypherlite`, verify §6 items 7 (dynamic rel types/labels via `:$(…)`), 3 (bytes-as-string interim), 11 (default-features), UNWIND $rows + CALL {} IN TRANSACTIONS + graph type/constraints + vector/fulltext SEARCH round-trip — **plus load tests**: bulk-load throughput (UNWIND batches of 1k/10k/100k), MERGE-heavy projection writes, and live-graph read latency at ~100k nodes / ~500k rels. Decision record.

DECISION: YES, but do load tests

2. **Store core**: `Store` on `cypherlite::Database` (open modes, txn wrapper with conflict retry, `:memory:` for tests), constraint/index DDL at open (replaces schema.sql chain).

DECISION: IS OK

3. **Module rewrites** in dependency order: store helpers → sync/content (repo_*) → projection (gf_nodes/edges, the centerpiece) → temporal → symbols/pins → memory pipeline (claims/entities/vectors) → jobs → connectors/oauth/account → deletion/maintenance (snapshot-based) → invalidation/doc_links (traversal-based).

DECISION: IS OK

4. **Migrator**: reviews.sqlite3 → CypherLite import tool.
DECISION: yes, via sql export to CSV and the import into cypherlite e.g. via load csv script

5. **Cleanup**: drop rusqlite + schema.sql files, FTS triggers, `ensure_column`; update Extension trait; docs.
DECISION: YES

## 8. Follow-up after the migration

### 8.1 Remaining SQL-authoritative domains (remove-sqlite was cancelled; the branch keeps rusqlite)

The removal node's blocked report left this precise inventory — rusqlite stays the runtime authority until these move to the graph store:
1. **Auth**: `workspaces`/`credentials` via `store.rs::authorize_hash` (~127 call sites through `Store::authorized*`); `create_workspace`/`grant`/`revoke` are SQL-only. The graph type declares `:Workspace`/`:Credential` but nothing writes them yet.
2. **Project registry reads**: `register_project` dual-writes a `:Project` node, but read paths still hit SQL `repo_projects` (folders.rs primary path, operations.rs, workspace.rs, api.rs, oauth.rs, vault.rs, auto_observe.rs, graph_projection.rs, fresh.rs, graph_command.rs, store::repo).
3. **Team/enterprise tables**: `am_members`, `am_sso`, `am_subscriptions`, `am_scim_tokens`, `am_browser_sessions`, `am_checkout_intents` (store_compat.rs, probed via sqlite_master).
4. **~10 smaller domains**: `am_model_profiles` (settings), `am_release_seen`, `am_binary_heads`, `am_document_reads`, `am_usage`/`am_call_reservations`, `am_deletion_receipts` + sqlite_master enumeration (deletion receipts' main path), repo_snapshots/objects/syncs prune + rusqlite backup/PRAGMA integrity_check (maintenance/local), `gf_episodes`/`gf_nodes` SQL reads (graph_projection, code_graph_arm, product, graph_tools, invalidation).
5. **it-fixture migration**: 14 test files still build SQL fixtures via rusqlite + `store::rows`.

After (a)–(e), the removal becomes a real deletion: drop the runtime rusqlite dep (keep it as a dev-dependency for the migrator's legacy-fixture test), delete the six schema files, remove `Store::open`'s SQL half and the dual backup paths.

### 8.2 One identity for a connector document (decided 2026-11-01)

`revocation::revocation_matches_credential_ids_exactly_not_as_substrings` still fails: it imports the *same* document id (`doc`) into two connectors of one repository, and the graph rejects the second with

```
ConstraintValidationFailed: `constraint_key_ConnectorDoc_repository_id_id` uniqueness
```

The three places that name a connector document disagree about what identifies it:

| place | identity | id it stores |
|---|---|---|
| SQL `am_connector_docs` | `PRIMARY KEY(connector_id, id)` | bare document id |
| `connectors.rs:727` (import) | `MERGE … {repository_id, connector_id, id}` | bare document id |
| `doc_links.rs:2031` (link state) | `MERGE … {repository_id, id}` | `connector:<cid>:<docid>` |

So a document id is unique per *connector* in SQL, and the graph type declares it unique per *repository* — the narrower key is what fails. `doc_links` is already right: it uses the `connector:<cid>:<docid>` form that `maintenance.rs` builds as a `SourceId` and that receipts show.

Widening the graph key to `(repository_id, connector_id, id)` is **not** a drop-in: `doc_links` also writes `ConnectorDoc` nodes for repository files (its `Document.connector` is `None` there), and the engine rejects a key whose property is absent — `ConstraintValidationFailed: … requires properties`, verified — so every one of those nodes would have to carry a sentinel. That leaves two coherent options, and the choice changes the receipt contract, so it is left open:

- **(a)** keep the key at `(repository_id, id)` and make `connectors.rs` store the same `connector:<cid>:<docid>` id `doc_links` and `maintenance.rs` already use — one node per document instead of today's two, but the `id` property changes, and `deletion.rs:2759` reports it as the `documents` column of a receipt.
- **(b)** widen the key to `(repository_id, connector_id, id)` with a sentinel `connector_id` (`''`) on the non-connector nodes `doc_links` writes — receipts keep their current ids, and `ConnectorDoc` keeps covering both kinds of document.

Until then the graph cannot represent two connectors of one repository holding a document of the same name.

**Decision (2026-11-01): option (b), SQL-faithful.** The test is a genuine identity case, not a fixture artifact: two distinct connectors (`Only A`, `Only AB`) each import a document `doc`, and the test expects both `connector:<cid>:doc` sources — exactly what SQL's `PRIMARY KEY(connector_id, id)` allowed. So the key is wrong, not the writer (a MERGE would have collapsed two connectors' documents into one node). The graph key is now `(repository_id, connector_id, id)`; `connectors.rs` keeps storing the bare document id, so receipts' `documents` ids are unchanged; `doc_links` link-state nodes carry the sentinel `connector_id = ''` (`store_graph::NO_CONNECTOR`), and `apply_schema` stamps that sentinel on any older link-state node before promoting the graph type (a no-op afterwards). Tradeoff rejected: option (a) would have changed the receipt contract and merged import and link-state nodes into one, so a link-state rewrite (`doc_links` deletes its own nodes) could take an imported document with it. Cost accepted: a sentinel value on non-connector nodes.

### 8.3 Remaining parity failures (11 of 12 fixed 2026-10-19)

The parity suite's failing tests were triaged one by one; the fixed ones were product bugs, not stale expectations, and are recorded in the commits `14f67711`, `201d48cd`, `c73c59ec`, `3f7c8cf3`, `d648aa79`:

| test | cause | state |
|---|---|---|
| `memory_graph::forgetting_a_connector_takes_its_memories_claims_and_embeddings` | `forget_dependent` left edge-less `Artifact` nodes | fixed |
| `memory_graph::memory_delete_cascades_through_the_graph` | `delete_memories` counted `MemoryRevision` nodes as removed and never deleted them | fixed |
| `memory_symbols::deleting_or_forgetting_a_memory_takes_its_symbol_pins` | same two as above | fixed |
| `receipts::memory_delete_signs_a_receipt_without_the_memory_and_verifies` | same revision gap — the receipt reported a removal that had not happened | fixed |
| `store_compat::deletion_state_treats_the_team_access_history_as_history` | `history()` probed the *graph* for the SQL-resident team tables | fixed |
| `project_identity::folder_locations_do_not_change_the_deletion_state` | `excluded.locations` reported the label, not the legacy table name | fixed |
| `project_identity::a_scratch_project_follows_its_folder_projects_access_and_deletion` | project deletion matched `EpisodeLabel` on `n.repo`; the projection writes `repository_id` | fixed |
| `receipts::source_retention_forgets_exactly_the_previewed_documents` | cited-source scan used a `-[:OF]->(:Project)` edge memories do not have, and did not decode the stringified `data` | fixed |
| `query_code_graph_arm::a_skipped_arm_reports_projection_state_and_episode_lag` | fixture set the job state with `UPDATE am_jobs`; jobs are `:Job` nodes | fixed (`608c001d`) |
| `retention::unresolved_calls_live_in_their_own_table_and_reload` | fixture counted the never-written `gf_unresolved_calls`; the calls are `:UnresolvedCall` nodes | fixed (`9bb44e01`) |
| `retention::twenty_five_episodes_are_trimmed_to_the_retention_window` | the trim filtered on `episode_sequence`, a property the projection never wrote; plus the fixture's own SQL counts | fixed (`3fb9ef12`, `173dddb7`) |

Open:

1. **`revocation::revocation_matches_credential_ids_exactly_not_as_substrings`** — the `ConnectorDoc` identity question in §8.2; **decided and fixed** (option (b)).
2. **`retention::twenty_five_episodes_are_trimmed_to_the_retention_window`** — **fixed.** Instrumenting the trim settled the open question: it ran on all 25 episodes and removed *nothing*, every time — because it selects unresolved calls with `WHERE u.episode_sequence <= $cutoff` while the projection wrote the nodes with `episode` only, an id string, no numeric `episode_sequence`, so the comparison was null. The projection now stamps the sequence (`3fb9ef12`), and the fixture's three remaining reads of the never-written `gf_unresolved_calls` table count the nodes instead (`173dddb7`) — including the retained-window semantics of the distinct-episode count (open and closed, not open only). Along the way the automatic trim also turned out to name its operation `retention.graph()` where everything else uses `retention.graph` (`d728a9f6`).

Items 2–4 are the it-fixture/store split of §8.1.5–8.1.4; item 1 is a design call.

**Parity after the eleven fixes** — the whole `it` suite, release, 4 threads, on an idle machine: **424 passed, 7 failed, 3 ignored in 1348 s** (before any of them: 411/20/3083 s in debug). The 7:

- the 2 open ones above (`revocation`, `retention`);
- **5 that are regressions, not pre-existing** — 3 `forgetting` and 2 `workflows`. The earlier note here called them "already failing before the migration"; running the same selection on the merge-base commit (`68501644`, in a detached worktree) gives **40 passed, 0 failed**, so the migration introduced them. Verified 2026-10-19. **3 of the 5 are now fixed** (`4f1a2758` restore cloned the graph sidecar twice; `288cd55f` a forgotten memory kept its claims, its revisions, and its history node's edges in the digest; `a1c07e36` the `Connector` node carried neither `workspace_id` nor `capability_hash`, so every scheduled sync job had an empty workspace, failed authorization, and was never leased). The 2 open ones are the `forgetting` pair below.

**The suite is load-sensitive, and the numbers above are for an idle machine.** A second full run under load average ~12 (28 users logged in, only the test binary consuming CPU) reported 413 passed / 18 failed / 3 ignored in 1570 s — and *all twelve* of its non-pre-existing failures pass when re-run alone (`--test-threads=1`: 12 passed, 0 failed, 60 s): `ask::ask_answers_with_basic_by_default_and_reflect_on_request`, `auto_observe::changing_the_observation_recipe_reprocesses_unchanged_sources`, `doc_links::an_upgraded_store_links_its_documents_at_the_next_drain`, `graph_always::basic_on_a_conversation_project_keeps_its_source_only_packet`, `grounded_v2::a_named_period_is_resolved_by_calendar_for_the_answer_model`, `jobs_bounds::independent_observers_wait_for_opt_in_and_disjoint_prefixes`, `observer::note_keys_reach_a_passage_through_the_notes_that_quote_it`, `observer::observer_notes_are_deleted_when_their_source_changes_or_is_removed`, `ux_terminal::ask_answers_with_numbered_sources_and_explains_model_errors`, `workflows::embeddings_feed_query_ranking_and_reuse_query_vectors`, `workflows::explicit_retry_preserves_failure_usage_and_repeated_clicks_reuse_the_child`, `workspace_scope::an_import_whose_other_end_changed_is_withheld_not_lifted`. Earlier runs showed the same behaviour with a smaller set (`auto_observe::sources_arriving…`, `doc_links::outcomes_across_projects…`, `memory_symbols::a_quote_across_symbols…`, `query_variants::caller_variants_fill_every_slot…`, all passing alone in 19 s). Read a loaded-machine run's failure count as an upper bound, and check any unfamiliar failure alone before treating it as real.

Release also runs the suite in a fifth of the debug wall-clock (1193 s vs 3083 s), which is the profile to use for it (see `docs/cypherlite-profile.md`).

**The upstream-feature port gaps closed 2026-10-26.** After the squash-rebase onto 0.1.4 the suite stood at 444/470 with ~14 failures in upstream's new features; all of them are now fixed on top of `1141adaf` (`26746705`, `8cba2e8d`, and the closing batch):

- `naming` (the `gf_doc_links` back-links) — the `LINKS_TO` query with the producers parsed from stringified JSON (`26746705`);
- `symbol_links::attach`/`links_of` — which memories name the route's symbols: their own `data.symbols` and their landed `PINNED_TO` pins, decoded from the stringified JSON (`26746705`);
- the lease hint's misfire (writes routed to the read-only follower after a transient holder) — the hint machinery is gone (`26746705`);
- the store-handle binding bug behind the `get_node` hang (§8.5) — `8cba2e8d`;
- `one_server` ×6, `lean_replies` ×2, `workspace_scope` ×3 (get_node ×2, the map counts) — all follow from the above plus the map's `symbols` count now reading `:SymbolHead` from the graph (the it-fixture's own `gf_symbol_heads` count went with it);
- `offboard` ×3 — the it-fixture's `holds`/`state` helpers read the backup's graph sidecar and the graph's `:Artifact`/`:EpisodeLabel` rows, `store_rows.repo_objects` became "the map is non-empty" (objects are CAS nodes in the graph now), and the cache purge in `maintenance::delete_project` no longer follows a link beside a backup file — a backup's scrub was deleting through a `gf-cache` symlink out of the data folder, exactly what the plan refuses.

Remaining open in the suite: the load-flaky set above. (The `forgetting` pair (§8.4) and `revocation` (§8.2) were resolved 2026-11-01.)

**Cross-repo incremental linking closed too (`99c3f5aa`).** The pass had been pinned to full (`incremental = false` — "no cross-link state to reuse", true mid-migration, stale since `gf_cross_link_state` was wired). Now the state row records inputs plus graph-counted live `CROSS_CALL`/`UnresolvedCross` rows (`link_counts`), `unchanged_link` reuses on a match, `previous_report` reads the graph so reused pairs report unchanged, and `graph_link_full` threads `full` for the CLI's `--full`. The whole `cross_repo` suite (8) passes.

### 8.4 Does forgetting re-derive what it removed? (decided 2026-11-01: yes)

`forgetting::deleting_a_connector_leaves_the_store_a_never_connected_fork_would_have` and `forgetting::forgetting_documents_leaves_the_store_a_never_ingested_fork_would_have` still fail, and they are the same question. Both build two stores that should end up identical — one forgets a connector's documents, the other never connected it — and compare their state digests. After the deletion the forgetting store is *missing* memories and claims the other has (the diff names them: "Comet now sleeps on the blue sofa", "Pegasus 41").

Both tests then run the extraction again (`work(...)`) before comparing, so the expectation is that the memories are **re-derived from the sources that remain**. They are not. The likely cause is job deduplication: an extraction job is keyed by `(repository_id, kind, input_hash)`, the job for that input already ran and completed before the deletion, so re-running is a no-op — the memories were deleted, but the job that would recreate them is considered done. Fixing it means deciding which is intended: forgetting re-derives (so the job key must account for deleted output, or the deletion must clear the job), or the fork comparison is too strict and the test should compare what the contract actually promises. That is a semantics call, not a patch.

**Decision (2026-11-01): forgetting re-derives — the SQL semantics.** The SQL `forget` deleted every `am_document_reads` row whose source was forgotten *or whose drafts named a lost memory or claim* ("an extraction read whose drafts were just removed must run again"), so the next `extract` recomputed the memories from the remaining sources and the fork comparison held. The job-key hypothesis above was wrong: `Job` nodes created since the documents became visible were already removed (`SINCE_VISIBLE`). The real gap was that the reads are still SQL-resident (§8.1.4) while the memories and claims moved to the graph: the legacy SQL half ran before the graph forget and saw no lost memories, and the graph half looked for `DocumentRead` nodes nothing writes. Now `deletion::forget` takes the SQL connection and clears the reads of the memories/claims it actually removed (`deletion::legacy::forget_reads`, the SQL predicate verbatim, shared by both halves). Tradeoff rejected: loosening the fork comparison to "what the contract promises" would have made certified forgetting weaker than the SQL store's guarantee (a forgetting store would permanently lose memories that a never-ingested store holds). Cost accepted: re-extraction spends model calls on the memories forgetting took. The documents test also had two stale coverage checks reaching past the old failure point (`Observation`/`am_vectors` — the digest names observer notes `am_observer_notes`, and passage embeddings live on the chunk nodes); they now check `am_observer_notes`, `repo_chunks` and the graph's vectors directly.

### 8.5 The store handle must outlive its connection (fixed 2026-10-26)

The `get_node` hang (and the earlier read-only-write failures it evolved from) was one bug: `let mut g = graph_for(db)?.connection()?;` binds only the *connection* — the `Arc<GraphStore>` is a temporary that drops at the end of the statement. The connection internally holds `Arc<Database>`, so the engine (and its writer lease) stays alive while the `GRAPHS` registry's `Weak` dies with the dropped handle. The next `graph_for` misses the registry, opens a second `StoreInner` for the same store, and its `Database::open` fights the zombie lease for the whole `LEASE_WAIT` window — 30 s per attempt, twice, per call. Symptoms: `database is read-only` on writes (the fallback path), a query thread parked in `engine()`'s lease-retry sleep, siblings blocked on the `GRAPHS` mutex held across the open.

Fix (`26746705` + the binding pass): hold the store for the connection's scope — `let store = graph_for(db)?; let mut g = store.connection()?;` — at every `.connection()` site whose store was a temporary (`doc_links`, `graph_projection`, `symbol_links::lift`, `symbol_links::attach`). Rule for new code: **never let the `GraphStore` handle be a temporary when taking a `connection()`**, and prefer `read`/`transaction` (pooled) over raw connections.

### 8.6 Architecture concepts

`architecture-concepts.md` — a jQAssistant-inspired enrichment layer (concepts/constraints as Cypher-applied labels, rel types, and properties; architecture rules in domain language; reports/diagrams; direct agent access to the architecture layer via MCP). Rules are authored as **Markdown documents with fenced rule blocks in the repo** — diffable, reviewable, rendered for the team — ingested at sync and applied as post-processing after each episode's projection; positioned as *architecture/enrichment skills* for agents. Designed against the CypherLite backend; to be planned with gbuild once this migration lands.
