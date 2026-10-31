# Changelog

All notable changes to CypherLite are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Zero-effect write notes on stderr (ADR-0402):** a batch statement (one-shot
  or piped stdin, never the interactive shell) that runs a mutation operator
  and applies no change prints `note: write statement changed nothing; the
  graph is unchanged` on stderr — with the statement's position in a
  multi-statement batch — and a `.commit` closing a transaction that attempted
  a write and applied nothing prints `note: .commit changed nothing; the
  transaction applied no changes`. The criterion is the engine's own effect
  record, so a catalog change that moves no mutation counter (`ALTER DATABASE
  … SET DEFAULT LANGUAGE …`, `CREATE GRAPH TYPE`, `CALL db.ontology.set/drop`)
  and a read-only statement the planner conservatively classifies as a write
  (`CALL { … }` with a read-only body, `LOAD CSV … RETURN`) stay silent. A
  rolled-back transaction keeps its per-statement notes; only the summary is
  suppressed. A statement whose render fails after the engine applied its
  change reports that failure (exit 1) and takes no note, while its real
  effect still joins the `.commit` summary — a persisted write is never
  described as having changed nothing. Exit codes and stdout are unchanged.
- **Engine write effect on every result (ADR-0402):**
  `QueryResult::effect` (`cypherlite::StatementEffect`) records whether a
  mutation operator reached execution and whether the engine applied a
  change, as one of three states — no attempt, attempted and applied
  nothing, applied — that merge as a lattice join for a transaction's
  ledger. The attempt reaches into `CALL { … }` and `WHEN/THEN/ELSE` bodies,
  and a batch rolled back by `IN TRANSACTIONS ON ERROR CONTINUE` leaves its
  changes out of that count, so a statement that applied nothing reports as a
  zero-effect write. It is execution metadata beside the token receipt, not
  announced result metadata: no wire envelope, transport or client-ABI field
  changes. The CLI's batch zero-effect write note reads it.
- **`--version` states the binary's license (ADR-0432, LABS-558):** both
  output shapes name the Apache 2.0 license of the `cypherlite` executable —
  the text form gains a second line, `License: Apache 2.0 (binary)`, and the
  `json` form gains the `license` member with the value
  `Apache 2.0 (binary)`. Neither shape prints the license text, and nothing
  else about `--version` changes (no config read, no database opened). The
  one pinned guarantee that does change is ADR-0193 Decision point 2: the
  text form as a whole is no longer byte-identical, so callers that pin
  `--version` stdout byte for byte — the `-v` spelling included — must
  expect the second line.

### Breaking

- **Logical-byte limits replace the token-named flags (no aliases).**
  `--memory-limit` is `--logical-graph-size-limit` and
  `CYPHERLITE_MEMORY_LIMIT` is `CYPHERLITE_LOGICAL_GRAPH_SIZE_LIMIT`
  (live graph + index cap; `SIZE` is bytes or `mb`/`mib`/`gb`/`gib`;
  `0` or unset is unlimited). `--concurrent-peak-token-limit` is
  `--concurrent-logical-workspace-limit` and
  `CYPHERLITE_CONCURRENT_PEAK_TOKEN_LIMIT` is
  `CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT`. The workspace limit
  is logical bytes: a former token capacity `N` is `N × 4096` bytes.
  The pool is required and positive when admission is on; the engine
  does not default it. `OpenOptions.memory_limit` is
  `logical_graph_size_limit` and `concurrent_peak_token_limit` is
  `concurrent_logical_workspace_limit`. `TokenEstimate` and
  `ExecutionReceipt` keep their type names; `peak_tokens` is
  `peak_logical_bytes`. The PROFILE key `PeakTokens` is
  `PeakLogicalBytes` (`TokenSchedule: v1` is unchanged). HTTP JSON
  `metrics.peakTokens` is `metrics.peakLogicalBytes` (Bolt plan metrics
  likewise); `metrics.tokenSchedule` is unchanged. The refusal name
  `concurrent-peak-token-limit` is `concurrent-logical-workspace-limit`.

## [0.13.0] — 2026-10-24

## [0.12.9] — 2026-10-22

## [0.12.8] — 2026-10-21


### Added

- **Bindings expose the plan document (LABS-418, ADR-0381):** the
  statement's structured plan as a `cypherlite.plan/v1` document — the
  operator tree with per-node estimates, PROFILE metrics (rows, db hits,
  time, memory, the read-kind mix and the walk's chosen start) and each
  operator's remedy-bearing warnings (ADR-0363) joined by operator id.
  Absent on a plain run. Python (`ResultSummary.plan` / `QueryResult.plan`),
  JS (`result.plan`), Java (`Result.getPlan()`), Go (`Result.Plan`), the
  WASM envelope (`plan`) and the MCP tool result (`plan`) — all the same
  document the C ABI's `cypherlite_result_plan_json` returns.
- **PROFILE gains an `Executed` column (LABS-343):** a plan where some
  operator never ran now marks each operator `✓` (ran) or `x` (never) in
  its own column, right after the operator's cells — gated to appear only
  when something never executed. An all-executed plan is unchanged.
- `cypherlite --width N` wraps the human `table` output to N display
  columns (LABS-399, ADR-0384): the widest columns shrink and their cells
  wrap at word boundaries over several lines, so a wide result fits the
  terminal. Without the flag the terminal width (`COLUMNS`) applies when
  stdout is a terminal, and a piped run keeps the natural width.

### Removed

- The per-column `.width` knob (its only writer, a dot-command, was
  withdrawn) and its truncate-with-ellipsis path — the width budget wraps
  instead (ADR-0384).
- The C ABI's `cypherlite_result_plan_json` returns a statement's structured
  plan as a `cypherlite.plan/v1` JSON document (ADR-0381) — the agent-native
  schema our own bindings hand out, distinct from the Neo4j-facing shape the
  Bolt and HTTP surfaces emit; `NULL` when the statement carried no plan.
- The plan document's per-node `warnings` carry the engine's remedy-bearing
  notifications (ADR-0363) matched by operator id — code, GQLSTATUS,
  severity, category, title, description and the actionable remedy — so an
  agent reads the diagnosis and the action beside it.

- **PROFILE names the walk's start and the store-read kinds (LABS-369,
  ADR-0375):** the plan table gains a `Reads` column breaking `Db Hits`
  into hops / neighbor walks / node reads (`135000h 0w 701n`), and a
  walking operator's `Details` cell names its chosen start
  (`[start #k, backward, bound-pivot]` — the element, direction and reason
  class). The same facts ride the machine modes and the Bolt/HTTP plan
  args (`Hops`, `NeighborWalks`, `NodeReads`, `StartElement`,
  `StartBackward`, `StartReason`); `cypherlite::ClauseMetrics` carries
  them as `reads`/`start`.

### Changed

- **`lab` importers take `-d` / `--database` (ADR-0383):** `cypherlite lab
  fs-to-graph` and `cypherlite lab git-to-graph` now name the store the way
  ADR-0200 names it on every other command. `--db` is removed rather than
  aliased: it is an unknown flag (exit 2, the `lab` usage line, no import, no
  migration hint). The target still defaults to `:memory:`, and an explicit
  flag still prints the summary, checkpoints a persistent target and exits
  without opening the interactive shell.
- **The `db.ontology.*` procedures require `CYPHERLITE_PREVIEW=ontology`
  (ADR-0378, LABS-402):** a call now fails while the preview feature is
  off, with
  `semantic error: SemanticError: preview feature 'ontology' is not enabled: set CYPHERLITE_PREVIEW=ontology`,
  and `SHOW PROCEDURES` omits the four procedures until the opt-in is
  set. The gate governs callability only: a stored ontology opens, replays
  and checkpoints with it off, and the document is served again once the
  store is reopened with the opt-in.
- **An invalid open-time configuration value rejected by `Database::open`
  exits 1, not 6:** an unknown `CYPHERLITE_PREVIEW` feature name (or an
  unparseable bulk-batch knob) is an open failure now, so it exits `1`
  (`fatal_error`) instead of `6` (`validation_error`). The stderr line is
  unchanged, as are the JSON error envelope's `kind` and `message`; the
  envelope's `code` and `exit_code` follow the open site, because no
  statement can fix the configuration.


## [0.12.7] — 2026-10-20

### Added

- **Query notifications (ADR-0363, ADR-0374, LABS-355):** `QueryResult`
  carries the flat notification list on the plan artifact it was derived
  with: `plan: Option<Box<cypherlite::PlanArtifacts>>` holds the tree
  (`PlanArtifacts::root`) and its `notifications` together, so a result can
  never carry notifications without the plan tree.
  `QueryResult::plan_root()` and `QueryResult::notifications()` are the read
  accessors (the list is the empty slice on a plain run). All twelve kinds
  ship.
  The six operator-scoped kinds — `CartesianProduct`,
  `UnboundedVariableLengthPattern`, `AllNodesScan`, `MissingIndex`,
  `EagerAggregation` and, on `PROFILE` only, `EstimationMiss` — are
  derived from the tree's pattern, operator and estimate facts and carry
  the raising operator's id. The six statement-scoped kinds —
  `UnknownLabelWarning` (01N50), `UnknownRelationshipTypeWarning`
  (01N51), `UnknownPropertyKeyWarning` (01N52), `DeprecatedFunction`,
  `DeprecatedProcedure` and `CallSubqueryWithoutScope` — read the store
  catalog, the key interner, the catalog's deprecation flags and the
  lowered IR's call scope. The three unknown-name kinds take read-side
  evidence only (decision 13): `MATCH` patterns, including the patterns
  nested in pattern predicates, comprehensions and `shortestPath`, and
  read property accesses; a `CREATE`/`MERGE` target pattern or a
  `SET`/`REMOVE` property target contributes nothing, while a read clause
  of the same write still reports. `EXPLAIN` and `PROFILE` populate the
  list; a plain run leaves it empty (no tree, no catalog walk). The one
  vocabulary table in `cypherlite-model` defines all twelve kinds with
  their code, GQLSTATUS, severity, category, one canonical title and
  canonical remedy; each record carries the instance description, an
  optional instance remedy and the raising operator's id. Two serializers
  project the record: the engine-native form (`code`, `gql_status`,
  `severity`, `category`, `title`, `description`, `remedy`,
  `operator_id`) and the Neo4j-shaped form (`code`, `title`,
  `description`, `severity`, `category`, with the remedy folded into
  `description` as ` (remedy: <remedy>)` and no `position` key). The
  shared success envelope always emits a `notifications` array — empty on
  a plain run — so `execute_wire`, `run_script_wire` and the C ABI JSON
  handle carry it.
- **CLI notification rendering (ADR-0363):** the CLI projects the engine's
  notification list into every output mode. `--mode table` prints one
  `Notifications:` block after the plan table on stderr — one line per
  entry, `<SEVERITY> <gql_status> <code>: <description> Remedy:
  <remedy>`, prefixed `Id <id>: ` on operator-scoped entries — excluding
  the three plan-warning kinds (`CartesianProduct`, `AllNodesScan`,
  `EstimationMiss`), which the plan table's `Warnings` column and footer
  already render, so a kind never prints twice (the block is absent when
  the filtered list is empty). `--mode markdown` appends the same block to
  the rendered rows with the complete list, plan-warning kinds included,
  because that mode prints no plan table. `--mode envelope` always
  carries the complete list under `notifications` in the native shape
  (empty array on a plain run); `--mode json`/`jsonl` carry it inside the
  `EXPLAIN`/`PROFILE` plan object, leaving plain row shapes unchanged;
  `--mode toon` prints a `notifications:` section beside `plan:`; `csv`
  and `cypher` render rows only. (feat(profile): surface the walk's start and the store-read kinds (LABS-369))
  and `cypher` render rows only.
- **`lab git-to-graph` (ADR-0376, OSK-35):** `cypherlite lab git-to-graph
  [DIR] [--db PATH] [--max-commits N] [--max-changeset N] [--since DATE]`
  imports a checkout's git history into a graph that joins the working-tree
  graph `lab fs-to-graph` builds, on the same `:File.path` keys: `Person`
  and `Commit` nodes with `AUTHORED` and `TOUCHED` edges, plus a
  `CO_CHANGED {count}` edge between files a commit changed together. Merge
  commits are skipped, `count` is set rather than incremented so a re-import
  is idempotent, and the import exits 1 when git is missing, DIR is not a
  repository, or git fails, and exits 2 on usage errors. The schema, path
  rule, indexes and flag defaults are pinned in ADR-0376; the question
  catalog ships at `references/lab-git-to-graph.md` in the CLI skill bundle.
- **`lab git-to-graph` resource bounds (ADR-0377, OSK-35):** an import is
  bounded before it writes anything. `git log` stdout above 134217728 bytes
  (128 MiB) fails with exit 1, more than 500000 distinct `CO_CHANGED` pairs
  fails with exit 1, and `--max-changeset` accepts at most 1000 (default 50;
  a larger value is a usage error, exit 2). `--max-commits` and `--since`
  read a smaller window and a lower `--max-changeset` shrinks each commit's
  pair product; a refused import leaves an existing target untouched.

### Fixed

- **The HTTP plan shape now matches the Neo4j Query API v2 (LABS-376):**
  plan nodes carry their metrics as fields (`dbHits`, `records`, `time`,
  the page-cache trio), the args map is named `arguments` (the Bolt wire's
  `args` is the driver's shape and stays Bolt-side), and `operatorType`
  carries the `@cypherlite` suffix. Browser-like tooling reads
  `arguments` — the old shape crashed its plan view with
  `Cannot read properties of undefined (reading 'string-representation')`.
  The CORS preflight allow-list also admits the tracing headers Browser
  sends (`sentry-trace`, `baggage`), which it rejected before any query
  ran.


### Breaking

- **One product binary (ADR-0368):** the standalone `cypherlite-bolt` and
  `cypherlite-mcp` binaries are retired (LABS-381). `cypherlite serve` is
  the one product binary: Bolt via `serve --host HOST --bolt PORT
  --database PATH`, MCP via `serve --mcp PORT` (streamable HTTP,
  unchanged) or the new `serve --mcp stdio` (foreground stdio transport
  until the host closes stdin; no auth on a stdio-only run). The old bolt
  spellings map: `--uri` → `--database`, `--bind HOST:PORT` → `--host HOST
  --bolt PORT`, `--identity` → `--bolt-identity`. Capabilities only the
  retired bolt binary exposed — TLS (`--tls-cert`/`--tls-key`/
  `--tls-client-ca`), `--load-csv-dir`, `--no-websocket`,
  `--ws-allow-origin`, the handshake/idle/query timeouts,
  `--max-result-bytes` and the storage knobs (`--memory-limit`,
  `--checkpoint-interval-secs`, `--durability-mode`, `--wal-dir`) — are
  library-level only (embed `cypherlite_bolt::serve`); for TLS, front
  `cypherlite serve` with a TLS-terminating proxy. The Docker image now
  carries `cypherlite` and `entrypoint` only (image name unchanged), and
  the `--entrypoint /bin/cypherlite-bolt` TLS alternative is gone.

- **Connection cap default (ADR-0333):** omitting `--max-connections` no
  longer stops at 128 connections per listener. Bolt, HTTP, and MCP share
  one process-wide cap, and the default is unlimited (`0`). A non-loopback
  bind logs a startup warning. Set `--max-connections` before exposing a
  port: every accepted connection takes a thread before authentication.

- **EXPLAIN result shape (ADR-0359):** `EXPLAIN <statement>` no longer
  returns a `plan` column of rendered string lines. It now returns the
  structured plan tree on `QueryResult.plan` with **no columns and no
  rows** — the plan rides the result alone; the tree carries strictly more
  information than the rendered lines it replaces. Scripts that read the
  `plan` column break; they read the tree (or the boundary-rendered text
  the CLI, Bolt and HTTP now compose) instead.

- **`PlanNode` text-rendering methods removed (ADR-0359):** the engine no
  longer renders plan text — `PlanNode::render_lines`,
  `render_with`, `render_lines_for_statement`, `render_header_for_statement`
  and `PlanRenderOptions` are removed, and
  `cypherlite/cypherlite/src/plan_node/render.rs` is deleted. The one plan
  renderer is now `cypherlite-formatter`'s public
  `plan_text_lines(tree, echo)`, `plan_header_lines(echo)` and
  `plan_presentation(tree)`, called at each boundary (CLI,
  Bolt, HTTP);
  `PlanNode::to_neo4j_value` now takes that boundary-rendered text as
  `plan_text: Option<&str>` (which heads its `string-representation`
  behind the `Cypher`/`Planner`/`Runtime` identity block).

### Changed

- **PROFILE's plan table goes to stderr in table mode (LABS-373):** the
  data table stays on stdout — pipeable, `jq`-clean — and the plan table
  prints on stderr, where a human sees it and a pipe does not. The machine
  modes keep carrying the plan inside their payload (`{"data":…,"plan":…}`).

- **PROFILE shows the plan beside the data (LABS-373):** a plan-carrying
  result renders the plan **accompanying** the data, never replacing it.
  `-mode table` prints the data table, a blank line, then the plan table
  (an `EXPLAIN`, which has no columns, prints the plan table alone);
  `-mode json` emits `{"plan":{…}}` for `EXPLAIN` and
  `{"data":[…],"plan":{…}}` for `PROFILE`; `-mode jsonl` emits the row
  lines then a final plan line; `-mode toon` emits the `rows[…]` block
  then the `plan:` block. The `string-representation` arg now shows the
  formatter's boxed operator table (`+---+` grid) — the same layout as
  table mode — in place of the old pipe-separated
  `Operator | Id | Details` form.

### Fixed

- **`AllNodesScan` warning named a property index as a label index
  (LABS-374):** the message read `the scan matches every node; a label
  index exists (:Movie(title))` — but `:Movie(title)` is a property index
  (including constraint-backed ones) and a label index (the internal
  label→node-id set) always exists, so the advice was meaningless. It now
  reads `the scan matches every node; consider the property index
  (:Movie(title))`, and the vocabulary is renamed from "label index" to
  "property index" throughout (`property_index_texts`, docs, tests).

- **`PROFILE` `Db Hits` counted only relationship traversal (LABS-375):**
  the store-access counter summed hop expansions and neighbor walks, so a
  node scan, a label scan and an index seek reported `0` db hits while
  doing real work. The node-store reads are now counted
  (`NODE_READS`, one per candidate a node match examined) and included in
  the total, so `PROFILE MATCH (n) RETURN count(*)` reports the scan's
  reads. `Memory (Bytes)` stays the row-materialization bytes: a
  streaming operator that materializes no rows reports `0` by that
  definition (documented in `docs/api/cli/spec.md`).

## [0.12.6] — 2026-10-18

### Fixed

- **`cypherlite update` 404 against the internal repository (ADR-0358):**
  the direct browser release-download route does not honor tokens for the
  internal repository, so it answered 404 for every asset and `upgrade`
  aborted with
  `Error: upgrade: GET https://github.com/neo4j-labs/cypherlite/releases/download/v0.12.5/SHA256SUMS: http status: 404`
  — with and without `GITHUB_TOKEN` set, so no installed binary could
  self-update past the failure. `upgrade` now probes the direct route
  first and, on a 404 with `GITHUB_TOKEN` set, falls back to the
  authenticated GitHub API asset route: a release-tag lookup resolves the
  three asset ids, then each asset is downloaded with
  `Accept: application/octet-stream` and the bearer token. Without a token
  the abort is one actionable line naming `GITHUB_TOKEN` and the
  documented reinstall one-liner. The SSHSIG-over-`SHA256SUMS` and
  checksum verification chain is unchanged, and the tokenless direct
  route resumes service once the repository is public.

## [0.12.5] — 2026-10-18

### Breaking

- **PROFILE result shape (ADR-0344/0345 phase 2, LABS-163):** the
  deprecated `_profile_plan`, `_profile_time`, `_profile_rows` and
  `_profile_mutations` columns are removed. `PROFILE <statement>` now
  returns the wrapped statement's own columns and rows unchanged, with
  the structured plan tree stamped beside them (`QueryResult.plan`)
  carrying per-operator metrics — `Rows` produced and `Time` elapsed —
  that the dispatch loop collected (an opt-in sink; plain queries pay
  one branch per clause, no clock). Mutation counters remain available
  on `QueryResult::stats`. The rendered plan table gains the
  `Rows | Time (ms)` columns on PROFILE trees.

### Added

- **Typed plan warnings (ADR-0344, LABS-163):** the EXPLAIN/PROFILE plan
  tree carries typed warnings per operator — `CartesianProduct` (a
  multi-pattern `MATCH` whose patterns share no join anchor),
  `AllNodesScan` (a label-less scan while a property index exists — the
  message names the existing index as the suggestion), and `EstimationMiss`
  (`PROFILE` only: an estimate overshooting the actual rows by 10x or
  more, the same policy that flags the `Est/Actual` cell). They render
  beside the affected operator in a `Warnings` column and in a
  `Warnings:` footer line after the table, and ride the machine-native
  wire form as a `warnings` list of warning names on the nodes that carry
  one.
- **PROFILE metric fields (ADR-0348, LABS-163):** the PROFILE plan tree's
  per-operator metrics gain `db_hits` — the operator's store-access
  count (hop expansions + neighbor walks, the delta of the executor's
  graph-access counters across the clause) — and `memory_bytes` — the
  row-materialization bytes, the summed logical size (ADR-0042) of the
  operator's produced rows (stateful operators' non-row state, like
  aggregation tables and sort buffers, is a follow-up). The rendered
  PROFILE table gains `Db Hits | Memory (Bytes)` columns, an
  `Est/Actual` ratio column on PROFILE trees that carry estimates
  (flagged `!` at a 10x-or-greater estimate overshoot), and the
  Neo4j-style `(never executed)` marker in the metric cells of an
  operator whose input clause produced no rows and that produced none
  itself — never zeros. The wire form's `args` carry `DbHits` and
  `MemoryBytes` beside `Rows`/`TimeNs` on every profiled operator.
- **Speedscope export mode (ADR-0349, LABS-163):** `--mode speedscope`
  exports a `PROFILE` statement's plan tree as one speedscope
  evented-profile JSON document, loadable at
  <https://www.speedscope.app> — one frame per operator
  (`"{operator} {id}"`, the tree's pre-order ids) and one open and one
  close event per operator, `at` values in nanoseconds. The timeline is a
  documented approximation: the tree carries per-operator inclusive times
  only, so children are laid out sequentially within their parent's
  interval and proportionally scaled down when their sum overflows it —
  the balance speedscope's importer requires. A plain query or an
  `EXPLAIN` statement is an error on stderr (exit code 1), never an
  empty profile.
- **Bolt plan depth-truncation marker (ADR-0353, LABS-346):** a
  `RUN` of an `EXPLAIN`/`PROFILE` statement whose plan tree nests past
  packstream's container depth limit (128) cannot pack the plan into the
  drained result's summary (ADR-0357's placement): the summary then carries
  `planTruncated: true`
  instead of the `plan`/`profile` key — distinguishable from a plain run,
  which carries neither. The marker is a Bolt-transport fact (the depth
  limit is that boundary's); the statement's own rows are unaffected.

### Changed

- **Plan summary placement and Neo4j-shaped plan args (ADR-0357, LABS-349):**
  the Bolt `EXPLAIN`/`PROFILE` plan (`plan`/`profile`) now rides the **final
  PULL SUCCESS** — and a `DISCARD` SUCCESS — beside `type` and `t_first`,
  Neo4j's placement, instead of the RUN SUCCESS: clients that read only the
  drained result's summary (cypher-shell) now render the plan. Every surface
  (Bolt, HTTP Query API, the CLI's machine modes) decorates the plan root's
  `args` with the Neo4j-facing vocabulary — `string-representation` (the
  rendered plan text, headed by the `Cypher`/`Planner`/`Runtime` lines
  tooling parses), `version`, the `planner*`/`runtime*` identity args
  (honestly named `cypherlite`) and `Memory` — and `EstimatedRows` mirrors
  as Neo4j's float form. The engine's own wire form is unchanged.

### Fixed

- **CLI table-mode `PROFILE` panic (LABS-346):** the table formatter
  treated any plan-carrying result as EXPLAIN-shaped string-line rows;
  a `PROFILE` result keeps the wrapped statement's own data rows, so
  `PROFILE RETURN 1` (or any PROFILE of a query returning non-string
  values) panicked. The plan presentation now reads its rendered text
  lines from the tree itself
  (`PlanNode::render_lines_for_statement`, the one echo rule), so
  `PROFILE` renders the plan table exactly like `EXPLAIN` in table mode.

## [0.12.4] — 2026-10-16

## [0.12.3] — 2026-10-15

### Added

- **CLI: did-you-mean command suggestions on parse errors (ADR-0230,
  LABS-303).** A statement that fails to parse and whose first word is a
  near-miss of a command (`cypherlite qu`, `telemetr`, `updat`, …) keeps
  the parse error as the headline and gains a `did you mean "query"? --
  ...` line below it, rendered from the same command table as `--help`.
  Strictly a fallback: valid Cypher with a command-like first token runs
  untouched, and the JSON error envelope is unchanged.

## [0.12.2] — 2026-10-14

### Added

- **CLI: `--format` output-flag spelling and `toon` output mode (ADR-0239,
  LABS-232).** `--format MODE` is the documented alias of `--mode` (the
  neo4j-cli spelling); `--output`/`-o` remain errors but now carry a
  did-you-mean that names `--format`. The new `toon` mode (shortcut
  `--toon`) prints a result set as one `rows[N]{col,…}:` header plus one
  comma-separated line per row, with nested values as inline JSON — the
  compact tabular form for agent contexts.
- **CLI: TTY-adaptive JSON and `--pretty` (ADR-0240, LABS-233).** `--json`
  output is now compact (one line) when stdout is piped or redirected and
  indented on a terminal; `--pretty` forces the indented form anywhere.
  `jsonl` and `envelope` were already compact and are unchanged.
- **CLI: `--timeout DURATION` for query runs (ADR-0241, LABS-234).** Each
  statement gets a wall-clock bound (bare seconds, or `ms`/`s`/`m` suffixes,
  fractions allowed; `0` disables) enforced through the engine's cooperative
  cancellation (ADR-0111). A timed-out statement prints `Error: query timed
  out after <duration>` and exits **7** — the formerly reserved row in the
  ADR-0194 exit-code map, now `timeout` — with `code: "timeout"` in the JSON
  error envelope.
- **CLI: `agent-context` discovery command and `skill list`/`skill check`
  (ADR-0242, LABS-236).** `cypherlite agent-context [COMMAND]` prints the
  compact agent briefing — command index, query-run flags, exit-code table,
  per-command synopsis/flags/examples — rendered from the same descriptor
  table as `--help`, so it cannot drift. It lives in the new `agent-context`
  Cargo feature (in the `full` and `agent` bundles). `skill list` reports
  each target agent's install state; `skill check` verifies installed
  bundles against the running binary's bundle and exits 1 on drift.

### Changed

- **CLI: `upgrade`/`update` discover releases from the website's
  `releases.json` (ADR-0250, LABS-316).** Release discovery now GETs the
  static `https://neo4j-labs.github.io/cypherlite/releases.json` and builds
  the platform-binary, `SHA256SUMS`, and `SHA256SUMS.sig` download URLs from
  the non-API route
  `https://github.com/neo4j-labs/cypherlite/releases/download/{tag}/{asset}`,
  so `upgrade` no longer depends on `api.github.com`'s 60/hour anonymous rate
  limit and has no API fallback (not even when `GITHUB_TOKEN` is set).
  `GITHUB_TOKEN` is now only an optional `Authorization: Bearer` header on
  the direct asset downloads — it is never sent on the metadata request.
  Missing, malformed, or `latest: null` documents abort with one
  `Error: upgrade: …` line naming the URL and the reinstall one-liner and exit
  1; a stale latest release keeps the "is up to date" line and exit 0. The
  SSHSIG verification (ADR-0115), version comparison, atomic self-replace, and
  skill refresh are unchanged. `CYPHERLITE_RELEASES_JSON_URL` overrides the
  metadata URL for dev/test.
- **Python: `Cursor.description` type codes re-sourced from the engine
  (ADR-0224, LABS-290).** The native `QueryResult` gains a `column_types`
  getter — one canonical engine type name per column (`STRING`,
  `INTEGER`, `FLOAT`, `BOOLEAN`, `NODE`, `RELATIONSHIP`, `PATH`, `LIST`,
  `MAP`, `POINT`, `DATETIME`, `DATE`, `TIME`, `DURATION`, `VECTOR`,
  `UUID`, `BINARY`, `ANY`), stamped from the first row (`ANY` for a `Null`
  cell or a result with no rows). `Cursor.description`'s `type_code` now
  maps those names onto the PEP 249 type-comparison objects
  (`STRING`/`BINARY` to themselves; `INTEGER`/`FLOAT`/`BOOLEAN` to
  `NUMBER`; `DATETIME`/`DATE`/`TIME` to `DATETIME`; every other name,
  including `ANY`, reports `None`). The client-side first-row inference
  (`infer_type_code` in `cypherlite/types.py`) is deleted.
- **CLI: bare `cypherlite` opens the interactive shell on a TTY (ADR-0231,
  LABS-304).** With no statement and no command word, an interactive
  terminal now drops into the same statement-session shell as `cypherlite
  query` with no `STATEMENT` (same prompt, `:memory:` default;
  `--database PATH` still selects the store). Piped or redirected stdin
  keeps the previous behavior — print the top-level help and exit 0 — so
  scripts never block. No flag or exit-code changes.

## [0.12.1] — 2026-10-13

### Added

- **C ABI: one canonical entry point per tabular operation (ADR-0220,
  LABS-288).** `cypherlite_connect_namespace(db, name, &conn)` now accepts a
  NULL `name` (selecting the default `neo4j` namespace), making it the single
  connect entry point; `cypherlite_query_params` with a NULL/empty params
  document is the single query entry point. `cypherlite_connect` and
  `cypherlite_query` remain as documented NULL-forwarding aliases and may be
  removed in a future major version. The JavaScript, Go and Java clients now
  bind only the canonical forms.
- **JavaScript: tagged-template queries (ADR-0222, LABS-291).**
  `conn.query` doubles as a template tag —
  ``conn.query`MATCH (p {name: ${name}}) RETURN p` `` — each interpolated
  value becomes an auto-named `$p0, $p1, …` parameter, injection-safe by
  construction. The same primitive value twice maps to one parameter
  (objects by identity); unsupported values — `undefined`, functions,
  symbols, `NaN`/`±Infinity`, non-plain objects — throw `TypeError` at
  any depth. The string form `query(cypher, params?)` stays the
  documented primary; the TypeScript types carry the overload.

- **Python: fuller parameter conversion (ADR-0219, LABS-291).** Query
  parameters now accept `uuid.UUID` (crosses as its canonical lowercase
  hyphenated string), `bytes`/`bytearray` — including the PEP 249
  `Binary` constructor — (as a `LIST<INTEGER>` of byte values 0–255, the
  lossless form while the engine has no binary scalar), and `tuple` (as a
  list, nested tuples included). Returned maps keep the engine's key
  order in the Python `dict`.
- **JavaScript: driver-style result ergonomics (ADR-0216, LABS-285).**
  `conn.query()` returns a `Result`: rows are objects keyed by column
  name (breaking change from positional arrays), `stats` carries the 11
  mutation counters, `single()` throws `ResultNotSingleError`,
  `consume()` returns a `ResultSummary` (counters, resolved namespace,
  query type, client-edge `elapsedMs`), and `graph()` returns a
  `ResultGraph` of the result's unique entities. The C ABI gains
  `cypherlite_result_stats`; the package ships hand-written TypeScript
  types (`index.d.ts`) and a bundled library skill
  (`.agents/skills/cypherlite/SKILL.md`).
- **Engine-reported per-statement elapsed time (ADR-0221, LABS-286).**
  `cypherlite::QueryResult` carries `elapsed` — parse, plan and run,
  stamped once in `Connection::run_query`, never around row
  serialization. The wire envelope (`response_envelope`) gains
  `elapsed_ms` next to `stats`; the C ABI gains
  `cypherlite_result_elapsed_ms(result) -> double`. Python's
  `ResultSummary.elapsed_ms` re-sources from the engine value (the switch
  ADR-0215 predicted, deleting the client-edge `perf_counter`),
  the JavaScript client's `consume()` `ResultSummary.elapsedMs` re-sources
  from the engine value, and
  the CLI's `.timer` `Time:` line reports the engine value. Bolt is
  unchanged: its `t_last` is the protocol's streaming duration, not
  statement elapsed.


- **Python: result graph extraction (ADR-0214, LABS-253).** `cursor.graph()`
  returns a `cypherlite.ResultGraph` — the unique `Node`/`Relationship`
  entities a result's rows carry (paths included, nested values walked),
  de-duplicated by id and id-ordered. `Node` and `Relationship` gain the
  driver-parity `element_id` and `items()` members. `cypherlite.viz` now
  shares the same extraction walk.

- **Python: driver-style result ergonomics (ADR-0213, LABS-253).** Cursor
  rows are `cypherlite.Record`s — `dict` subclasses with attribute access,
  `.data()` and column-ordered `.keys()` — and cursors carry the Neo4j
  driver's `Result` conveniences: `data()`, `fetch(n)`, `peek()`,
  `single()` (raising the new `ResultNotSingleError`), `keys()`, and
  `to_df()` for a pandas DataFrame (optional `neo4j-cypherlite[pandas]`
  extra; a missing install raises a helpful `ImportError`). The DB-API
  contract is unchanged.
- **Python: `cursor.to_arrow()` and PEP 561 typing.** `to_arrow()`
  returns the remaining rows as a `pyarrow.Table` (optional
  `neo4j-cypherlite[arrow]` extra; `.to_pandas()` and `pl.from_arrow()`
  both work on it; an empty result keeps the column shape). The wheel
  ships `py.typed`, so type checkers see the binding's annotations.
- **Python: bundled library skill.** The wheel ships an Agent Skills-format
  skill at `cypherlite/.agents/skills/cypherlite/SKILL.md`
  (library-skills.io) — `uvx library-skills` installs it for AI agents,
  version-synced with the package.

### Changed

- **Python: points are explicit-only (ADR-0219, LABS-291).** A plain
  dict with `srid` plus `x`/`y` or `longitude`/`latitude` keys is no
  longer silently reinterpreted as a point parameter — dicts are always
  maps. Pass `cypherlite.Point(srid, x, y)` or build the point in Cypher
  with `point($coords)` instead. Parameter-shape errors are now
  DB-API-worded: a non-dict `params` or a non-string key raises
  `cypherlite.ProgrammingError` with a message that says so (previously
  pyo3-internal wording).
- **Breaking wire change: entity values carry a `kind` tag (ADR-0223,
  LABS-289).** Nodes on the wire are now
  `{"kind":"node","id","labels","properties"}` and relationships
  `{"kind":"relationship","id","type","start_node","end_node","properties"}`,
  so clients dispatch on a documented discriminator instead of
  shape-sniffing key sets. Paths are unchanged (their elements carry the
  tags). Readers that ignore unknown keys are unaffected; strict
  key-set matchers must accept the new field. This supersedes the wire
  format's earlier "no type tags" promise (pre-1.0/alpha, same precedent
  as ADR-0280).

## [0.12.0] — 2026-10-12

### Added


- **`db.ontology.*` procedures (ADR-0120, ADR-0121, ADR-0226, ADR-0227,
  ADR-0228, LABS-132).** Neo4j's CIP-287 surface manages the store's single
  ontology document, acting on the session/`USE`-target database (which must
  be `neo4j`): `db.ontology.set(ontology)` validates a JSON or YAML document
  against the pinned Ontology Graph Spec 1.0.0 and stores the submitted
  bytes verbatim (16 MiB cap); `db.ontology.get(format = "yaml")` returns
  the stored document unchanged when the format matches and canonically
  converted otherwise (`null` when none is set); `db.ontology.drop()`
  removes it; `db.ontology.apply()` converts it to a graph type and applies
  it with `ALTER CURRENT GRAPH TYPE SET` semantics, failing when existing
  data violates the result. `set`/`drop` run in implicit transactions only.
  The stored document is the sole source of truth and is reachable only
  through the procedures: the `system_ontologies` namespace stays listed by
  `SHOW DATABASES` but is not queryable (ADR-0229) — every statement routed
  to it, reads included and `EXPLAIN`-wrapped included, fails with 42N17
  `This database is not queryable. Use the db.ontology.* procedures on the
  user database 'neo4j' instead.`; only the ADR-0116 §2
  database-administration commands still run from there, and no graph is
  derived from the document. The document persists as an immutable
  snapshot object (Parquet manifest v4) or an inline `graph.json` field
  (`value_encoding` 3), with WAL segments rotating to the new `MDBWAL02`
  magic while a store holds one; stores that never set an ontology are
  byte-for-byte unchanged, and older binaries fail closed on
  ontology-carrying stores. See the
  [ontology procedures reference](docs/reference/ontology-procedures.md).
- **Store path is `--database` (ADR-0200, LABS-247).**
  `cypherlite query [STATEMENT] [--database PATH]` is the explicit
  statement-session verb, aligned with `neo4j-cli query`. The same flag is
  the store on the path-first form
  (`cypherlite [OPTIONS] [--database PATH] [QUERY]`), on `pull`, and on
  `info`. The first positional is the statement, not the database. `-d` is the
  short form of `--database` on those same commands. Default store remains
  `:memory:`. Present in every bundle. With no statement and no `query`
  verb, the process prints help and exits. `query` with no statement is the
  stdin script or the interactive shell.

## [0.11.8] — 2026-10-10

### Changed

- **Query-run exit codes and a JSON error envelope (ADR-0194, LABS-229).** A
  rejected statement (syntax, semantic or type error, including a write on a
  `--read-only` database) now exits `6` and a write conflict or a database
  locked by another opener exits `5`; other runtime failures still exit `1`.
  With `--json` or `--jsonl`, a failure also prints
  `{"error":{"code","exit_code","kind","message","retryable"}}` on stdout. The
  stderr line is unchanged. Scripts that compare the exit status to exactly
  `1` must accept `5` and `6`; zero/non-zero checks are unaffected.

### Added

- **Progress report for a failed multi-statement run (ADR-0199, LABS-240).**
  When `-c` or piped input holding several statements stops on a failure, stderr
  gains `statement 2 of 3 failed; 1 completed and committed; 1 not run` and the
  JSON error object gains `statement`, `statements` and `committed` (`0` inside
  an open `.begin` transaction). Single statements and successful runs are
  unchanged. Execution is unchanged: it still stops at the first failure.
- **Syntax-error positions (ADR-0290, LABS-239).** A syntax error's source line
  and caret are on their own lines on stderr again, under the unchanged first
  line, and the error object in `json`/`jsonl`/`envelope` modes gains `column`
  and `source` (and `line` when the parser reports only a position). The fields
  are absent when the position is unknown.
- **Examples in `--help` (ADR-0197, LABS-237).** The top-level help and every
  command's help end with one or two examples. `CYPHERLITE_HELP_EXAMPLES=0`
  hides them; nothing else on the page changes.
- **Row cap for agent runs: `--max-rows N` (ADR-0196, LABS-230).** Under a
  detected agent harness (Claude Code, Codex, Cursor, Gemini CLI, …) with stdout
  not a terminal, a statement now prints at most 100 rows, warns on stderr and
  exits `0`; `--mode envelope` reports `"truncated": true`. `--max-rows 0` or
  `CYPHERLITE_MAX_ROWS=0` lifts it; any explicit value is honored everywhere.
  Terminals, interactive shells and scripts outside a harness are unchanged.
- **`--mode envelope` (ADR-0195, LABS-231).** Prints
  `{"columns":[…],"rows":[…],"truncated":false}` per result set on one line;
  an empty result still lists its columns. `--json` and `--jsonl` are
  unchanged. A failure carries the error envelope, as in `json`/`jsonl`.
- **`--version --json` (ADR-0193, LABS-238).** `cypherlite --version --json`
  prints `{"arch","name","os","version"}` as one JSON line; the text form is
  unchanged.
- **"Did you mean" for unknown flags (ADR-0192, LABS-242).**
  `cypherlite --readonly …` now answers `Did you mean '--read-only'?` before
  the usage line, and every command does the same for its own flags
  (`cypherlite clone --wal-dri …` suggests `--wal-dir`).

## [0.11.7] — 2026-10-09

## [0.11.6] — 2026-10-08

### Added

- **Unquoted Unicode identifiers (ADR-0182, LABS-188).** Variables, labels,
  relationship types, property keys, map keys, aliases and parameter names
  accept non-ASCII characters under the CYPHER 25 rule: a BMP-only Unicode
  13.0.0 `General_Category` predicate (`Lu`, `Ll`, `Lt`, `Lm`, `Lo`, `Nl`,
  `Pc` at the start; `Mn`, `Mc`, `Nd`, `Sc` added for later characters) minus
  `Pattern_Syntax`, backed by a checked-in generated table
  (`cypherlite-parser/src/unicode_identifier/tables.rs`, generated by
  `scripts/gen-unicode-identifier-tables.py`). `RETURN 1 AS café`,
  `MATCH (n:Ärzte)`, `-[:BESCHÄFTIGT]->`, `n.Größe` and `$größe` now parse;
  `ſKIP` (U+017F) stays an identifier, never a keyword; `RETURN 1é` stays an
  `InvalidNumberLiteral`; and identifiers remain code-point-exact (no
  normalization or case folding). A non-ASCII character that is not a valid
  identifier start keeps the `InvalidUnicodeCharacter` error.
- **Read-only open is available on Bolt, Python, C, JavaScript, Java, Go, and WASM (ADR-0136).** Each stays writable by default; the opt-in loads once, refuses writes and checkpoints, and does not create a missing store.

### Changed

- **CLI help is scoped to the command (ADR-0190, LABS-202).** Top-level
  `cypherlite --help` is shorter: two usage lines, a `Commands:` index of the
  commands compiled into the binary, the query-run options, and a closing
  `Run 'cypherlite <COMMAND> --help' for details on a command.` line;
  lifecycle-command flags moved into each command's own help.
  `cypherlite <command> --help` (e.g. `cypherlite clone --help`) prints that
  command's synopsis and flags to stdout and exits 0. A usage error prints
  exactly three lines to stderr — `Error: …`, the failing command's `Usage:`
  line, and a `--help` hint — and stays exit 2.
- **`db.schema.visualization()` virtual relationships now carry a `name`
  property equal to their relationship type (ADR-0187, LABS-185).** Each
  virtual relationship in the result's `relationships` list has exactly one
  property, `name`, a `STRING` equal to `type(r)`, matching Neo4j's shape;
  the virtual nodes, negative ids, `startNode()`/`endNode()` resolution,
  sorting and zero-rows empty behavior are unchanged.
- **Writes under relationship `UNIQUE`/`RELATIONSHIP KEY` constraints and
  under composite `UNIQUE`/`NODE KEY` constraints no longer slow down as the
  constrained relationship type or label grows (ADR-0184, ADR-0185,
  LABS-180).** A relationship uniqueness probe now reads the lazy
  relationship equality postings plus the scratch's twin — one equality
  bucket per touched tuple — instead of walking every live relationship of
  the type, and a composite probe picks the constrained property whose
  committed-plus-scratch bucket is smallest. Accept/reject decisions, error
  texts and the inspection counter's per-entity readings are unchanged.

### Fixed

- **`timestamp()` now evaluates on the statement clock (LABS-182).** The
  builtin returns the statement's pinned instant as epoch milliseconds — one
  value for the whole statement, equal to `datetime().epochMillis` — instead
  of failing with `unknown function timestamp()`. Calls with arguments fail
  with `InvalidNumberOfArguments`.
- **A bad `CYPHERLITE_MEMORY_LIMIT` is rejected like a bad `--memory-limit`.**
  The CLI exits 2 with `Error: invalid CYPHERLITE_MEMORY_LIMIT: …` and does
  not open the database. Help, version, and lifecycle commands do not read
  the variable. Bolt startup rejects the same value through the
  flag parser. `Database::open` returns that semantic error instead of
  treating the value as unlimited. `0`, a blank variable, and an omitted
  variable stay unlimited, and an explicit `--memory-limit` still wins.
- **Keywords parse unquoted as aliases, variable references, binding
  variables and map keys (ADR-0191, LABS-183).** `WITH 1 AS a RETURN a AS
  as` and `WITH [1] AS s RETURN s AS union` failed with `expected alias
  after AS, got "as"` / `"union"`, so Neo4j-written Cypher such as the
  graph-examples recommendations walkthrough did not parse. Any keyword is
  now a valid alias after `AS`/`YIELD`, variable reference, binding variable
  (patterns, `FOREACH`, comprehensions, quantifiers, `reduce`, `CALL`
  import) and map key (map literals, map projections, pattern property
  maps), kept exactly as written. The existing meanings keep priority: the
  literals `TRUE`/`FALSE`/`NULL`/`INF`/`INFINITY`/`NAN`, prefix `NOT`,
  `DISTINCT`/`ALL` directly after `RETURN`/`WITH` and inside `fn(`, the
  `CASE`/`COUNT`/`EXISTS`/`COLLECT` forms, the inline element `WHERE`, and
  the `n IS Not` negation (ADR-0169). Generated Cypher keeps backticking
  reserved words.
- **The always-zero `graph write wait` row (`graph_write_lock_ns`) was
  removed from the commit-timing report (LABS-190).** The row timed two
  adjacent statements rather than a lock, so it reported ~0 on every
  commit. The report's other rows — parse+plan, overlay execute, wal_lock
  wait, encode+write, `sync_all` and `apply_redo` — are unchanged.
- **The cookbook's navigation keeps its GitHub item (ADR-0288).** Opening
  Cookbook from the landing page no longer drops GitHub from the navigation
  bar: every cookbook page links the repository, as the landing page does,
  while it is still internal. The cookbook's other repository links (footer,
  sources, ADRs, companion programs) still wait for publication (ADR-0133),
  and the build's link check accepts exactly the repository URL.

## [0.11.5] — 2026-10-07

### Changed

- **Breaking: `cypherlite::Value` shares its String, List, Map and Vector
  payloads behind `Arc` (ADR-0154, ADR-0170, LABS-156).** The variants are
  now `Value::String(Arc<str>)`, `Value::List(Arc<Vec<Value>>)`,
  `Value::Map(Arc<BTreeMap<String, Value>>)` and
  `Value::Vector { coord, values: Arc<Vec<Value>> }`, so cloning a value
  shares its payload in O(1) and values are immutable. Variable reads and row
  carries of these values become constant-time instead of copying the whole
  value, removing the quadratic cost of queries that index into a large
  collected list once per row. Rust code that constructs or destructures the
  four variants wraps owned payloads with `.into()` or `Arc::new(..)` and
  takes them back with `Arc::unwrap_or_clone`; no helper functions are
  added. The C ABI, Bolt, the Python, JavaScript, Java, Go and WASM clients,
  and persisted WAL/JSON/Parquet bytes are unchanged.
- **The Bolt `--max-result-bytes` check stops once the result exceeds the
  limit and runs on the engine blocking pool (ADR-0110, LABS-156).** Rows that
  share one large payload are cheap in memory but large in logical bytes, so
  summing every row could stall the server's async workers for seconds,
  outside `--query-timeout-secs`. Such results now fail at once with the same
  `query result exceeds max-result-bytes limit` FAILURE; the measure (logical
  bytes) and `0` to disable are unchanged.
- **Bolt parameters decode driver temporal and point structs, and unsupported
  parameter values fail the RUN (ADR-0177, LABS-189).** The nine Bolt 4.4
  struct tags — `0x44` Date, `0x54` Time, `0x74` LocalTime, `0x46`/`0x66`
  legacy DateTime/DateTimeZoneId, `0x64` LocalDateTime, `0x45` Duration,
  `0x58`/`0x59` Point2D/Point3D — now decode to `date`, `time`, `localtime`,
  `datetime`, `localdatetime`, `duration` and `point` values instead of
  silently becoming `null`. Values the engine cannot represent now fail the
  RUN with a FAILURE under `CypherLite.Error` instead of being dropped:
  `Bytes`, an unknown struct tag, a non-string map key at any depth, and a
  non-map params field. Result encodings are unchanged (ADR-0272).

### Fixed

- **A non-ASCII character outside strings and comments lexes as one
  `InvalidUnicodeCharacter` error, not byte-wise mojibake (LABS-188).** The
  lexer's fallback arm cast each byte of a multi-byte UTF-8 character to
  `char`, so `RETURN 42 — 41` (U+2014 em dash) produced three error tokens
  rendering `unexpected character: â` and never surfaced the TCK's
  `InvalidUnicodeCharacter` detail. The fallback now decodes the full UTF-8
  character, consumes its bytes, and raises one error tagged
  `InvalidUnicodeCharacter` whose message carries the decoded character; the
  retired TCK skip moves to the allowlist (3892 of 3893 pass, one documented
  skip). ASCII characters keep the untagged `unexpected character:` error.
- **Keywords parse unquoted as label and relationship-type names, as in
  Neo4j (ADR-0169, LABS-154).** `MATCH (o:Order) RETURN o` failed with
  `expected a label name, got "Order"`, so Neo4j-written Cypher such as the
  northwind walkthrough did not parse. Any keyword is now a valid label or
  type name in patterns (`MATCH`/`CREATE`/`MERGE`), label predicates
  (`n:Order`, `n IS Order`), `SET`/`REMOVE` labels, label expressions
  (`:A|Order`, `:!Order`) and schema/hint label positions, kept exactly as
  written; backticked names behave as before. A keyword directly after
  `:`, `IS`, `|`, `&` or `!` is always read as a label. Keywords used as
  variables, aliases and map keys still need backticks, and generated
  Cypher keeps backticking reserved words.
- **`toInteger()` / `toFloat()` handle NaN, Infinity and out-of-range
  input like Neo4j (ADR-0168, LABS-177).** Floats keep saturating, as the
  Neo4j 2026.09 probe confirmed: `toInteger(0.0/0.0)` is `0` and
  `toInteger(1.0/0.0)` is `9223372036854775807`, also in
  `toIntegerOrNull()` and `toIntegerList()`. A numeric string is now judged
  by its exact decimal value instead of an `f64` parse: outside the INTEGER
  range `toInteger()` raises `type error: NumberOutOfRange: toInteger() …`
  and `toIntegerOrNull()` returns `null` (before, `toInteger('1e300')`,
  `toInteger('9.3e18')` and `toInteger('9223372036854775807.5')` saturated
  to `9223372036854775807` and `toInteger('1e400')` was `null`); in range
  it truncates without `f64` rounding, so
  `toInteger('9007199254740993.5')` is `9007199254740993` (was
  `9007199254740994`). `toIntegerList()` keeps INTEGER list elements
  exact: `toIntegerList([2.5, 9007199254740993])` is
  `[2, 9007199254740993]` (was `[2, 9007199254740992]`); VECTOR
  coordinates still convert through FLOAT. `toFloat()` /
  `toFloatOrNull()` accept only Java's non-finite spellings, exactly `NaN`
  and `Infinity` with an optional `+`/`-`: `'nan'`, `'NAN'`, `'inf'`,
  `'Inf'`, `'+inf'`, `'infinity'` and `'INFINITY'` now return `null`
  (they were NaN or ±Infinity), while `toFloat('1e400')` is still
  Infinity.
- **String list elements, Java string forms, and the remaining
  float-to-integer sites follow Neo4j (ADR-0175, LABS-178).**
  `toIntegerList()` / `toFloatList()` convert each element independently
  with the scalar `OrNull` rules instead of nulling every string element:
  `toIntegerList(['1', 'NaN'])` is `[1, null]` (was `[null, null]`) and
  `toIntegerList([true, false])` is `[1, 0]` (was `[null, null]`); a
  string element that fails to parse becomes a `null` element, never an
  error. The scalar string parsers accept Java's forms: surrounding
  whitespace (`toFloat(' 1.5 ')` is `1.5`, was `null`), `f`/`F`/`d`/`D`
  suffixes (`toInteger('1.5f')` is `1` and `toFloat('2f')` is `2.0`,
  both were `null`), hex floats (`toFloat('0x1p3')` is `8.0`, was
  `null`), and the Cypher25 hex/octal/underscore literals
  (`toInteger('0x1F')` is `31` and `toInteger('1_000')` is `1000`, both
  were `null`). A decimal exponent beyond the 32-bit range parses as a
  float literal, superseding ADR-0168's deliberate deviation:
  `toInteger('1e3000000000')` saturates to `9223372036854775807` (was a
  `NumberOutOfRange` error). `range()`, `substring()`, `left()`,
  `right()` and list-slice bounds coerce float arguments (truncate toward
  zero, NaN → `0`, saturate) instead of erroring or mis-truncating:
  `range(head([1.5, 'x']), 5)` is `[1, 2, 3, 4, 5]` (was an
  `InvalidArgumentType` error), `substring('abcdef', head([1.5, 'x']))`
  is `'bcdef'` (was `'abcdef'`), `left('abcdef', head([2.9, 'x']))` is
  `'ab'` (was `''`); results outside [0, 2147483647] raise
  `NumberOutOfRange`, and float *literals* at these sites now fail before
  execution (`substring('abcdef', 1.5)` was `'abcdef'`, now an
  `InvalidArgumentType` error). `SKIP` / `LIMIT` and list index reject
  every float, `1.0` included: `... SKIP head([1.0, 'x'])` returned rows
  and `[1,2,3][1.0]` was `2`; both now raise `InvalidArgumentType`.
  `(-9223372036854775807 - 1) / -1` wraps to `-9223372036854775808` and
  `(-9223372036854775807 - 1) % -1` is `0` (both were
  `integer out of range` errors), replicating Neo4j's unchecked Java
  operators.
- **`point()` accepts node and relationship arguments, and its map rules
  match Neo4j (ADR-0167, LABS-169).** `point(l)` on a `Location` node failed
  with `type error: point() requires a map` because only `Value::Map` was
  accepted. Nodes and relationships are now read as their property maps
  (freshly resolved through the graph, like `properties()`), extra
  properties are ignored, and the shared core follows the docker-verified
  Neo4j semantics: cartesian iff both `x` and `y` are present (`height`
  ignored), else geographic iff both `latitude` and `longitude` are present
  (`z` wins over `height`); an explicit `srid` must be an integer in
  {4326, 4979, 7203, 9157} matching the coordinate dimension (a float or
  string `srid` was previously ignored silently); latitude must be in
  [-90, 90] and longitude wraps into [-180, 180].
- **`lab fs-to-graph` no longer opens the interactive shell when `--db` is
  set (ADR-0286, LABS-166).** `cypherlite lab fs-to-graph DIR --db PATH`
  imported the tree and then, on a TTY, dropped into the `cypher>` prompt.
  An explicit `--db` now prints the import summary and exits; a persistent
  target is still checkpointed. Omitting `--db` still opens the shell on a
  TTY (with `repl`) so the in-memory graph can be queried before the
  process exits.
- **A non-literal `LOAD CSV … FIELDTERMINATOR` is a syntax error instead of
  being silently ignored.** `FIELDTERMINATOR 1` or `FIELDTERMINATOR $sep`
  parsed, then dropped the value and split rows on the default comma. The
  terminator must now be a string literal (`FIELDTERMINATOR '\t'`), as in
  Neo4j's grammar; anything else fails with `expected a string literal after
  FIELDTERMINATOR`.

## [0.11.4] — 2026-10-05

### Added

- **`cypherlite info DATABASE [--json]`, the `.info` dot-command, and the
  public `cypherlite_storage::store_info` accessor (ADR-0165, LABS-174).**
  `cypherlite info` answers "what is in this store?" with a cold,
  bounded read — no `Wal::open`, no shard load, no WAL replay, no lock
  acquisition — so it runs beside a live writer: layout (native directory
  or `.cl` bundle, with the superblock version), snapshot codec and
  format version, checkpoint LSN and commit timestamp, total size on
  disk, WAL segments past the checkpoint (a staleness hint),
  checkpoint-time node/rel counts, and the index/constraint catalog,
  printed as a key/value table or one JSON object with `--json`. A fresh
  store before its first checkpoint reports `StoreSnapshot::None` — a
  state, not an error. In a session, the new `.info` dot-command (the
  eighth, compiled into every bundle) prints the same store block plus
  exact live counts and the current catalog. The command is gated on the
  new `info` Cargo feature (in the `full` and `agent` bundles); the Rust
  surface is `cypherlite_storage::store_info` returning `StoreInfo` with
  `StoreLayout` and `StoreSnapshot`.

## [0.11.3] — 2026-10-04

### Changed

- **Breaking: `clone` covers the full dir/`.cl` matrix; the `snapshot`
  command and its engine API are removed (ADR-0282, LABS-168).**
  `cypherlite clone SOURCE DEST` now moves a database between any two
  stores — dir → dir, dir → `.cl`, `.cl` → dir, and `.cl` → `.cl` — with
  each side's format inferred from the `.cl` suffix on its path. In the
  Rust API, `Database::clone(source, dest, CloneOptions)` replaces
  `Database::clone_directory`, `Database::snapshot_to_bundle`,
  `Database::restore_from_bundle`, and `SnapshotOptions`;
  `CloneOptions.wal_dir` is rejected with `Error::Storage` when the source
  is a `.cl` bundle (a bundle embeds its WAL). The `cypherlite snapshot`
  command is removed outright — it shipped in internal releases only, so it
  breaks cleanly with no deprecation shim — and a database path literally
  named `snapshot` is openable again without the `./` / `--` escapes.
  `clone` also gains `--to-snapshot-format json|parquet` (ADR-0163), with
  `CloneOptions.to_snapshot_format` in the Rust API: the destination's
  checkpoint codec, overriding the codec-preservation default to transcode —
  e.g. a compact Parquet bundle from a JSON working directory. A codec the
  build does not include fails before the destination is created.

## [0.11.2] — 2026-10-03

## [0.11.1] — 2026-10-02

### Changed

- **Breaking wire change: non-finite floats render as strings in JSON
  (ADR-0280, LABS-165).** `NaN`/`+Inf`/`-Inf` previously encoded as JSON
  `null` in every JSON surface (CLI `--json`/`--jsonl`, the canonical
  `cypherlite_model::wire` encoding behind the C FFI, the Python bindings,
  the WASM/binding envelopes, Arrow fallbacks, and cookbook recordings) —
  indistinguishable from a genuine null. They now encode as the JSON strings
  `"NaN"`, `"Inf"`, and `"-Inf"` (the `CypherValue` `Display` spellings), so
  JSON modes agree with table/CSV modes. Bolt (PackStream `f64`) and the
  `.mode cypher` literal renderer are unaffected. Acceptable pre-1.0/alpha
  per the wire module's documented breaking-change precedent.
- **`DELETE inf`/`DELETE nan` diagnostic names the cause (ADR-0280,
  LABS-165).** A bare `inf`/`infinity`/`nan` DELETE target is the CIP-55
  float-literal keyword, not a variable; the compile-time rejection (still
  `InvalidArgumentType`, per TCK Delete5 scenario 9) now says so and
  prescribes the backtick escape (`` DELETE `inf` ``) instead of the bare
  "DELETE requires a node or relationship, not an expression".

## [0.11.0] — 2026-10-02

## [0.10.1] — 2026-09-30

### Changed

- **Cookbook graph view drawn with NVL, interactive (ADR-0151).** A step's
  graph result is drawn by NVL, Neo4j's visualization library, instead of a
  static d3-force SVG. Readers zoom (pinch, the wheel once they have clicked
  into the graph or with Ctrl/⌘ held, or the buttons), pan, drag nodes,
  hover or click an element to see its labels or type and its properties,
  fit the graph to the pane, and resize the pane. Double-clicking a node
  adds its neighbours (up to 100) and their relationships from the page's
  in-browser database, running the earlier steps first when they have not
  run; a button puts the step's result back. NVL lays out on the page's
  thread (its workers are off) and its telemetry is off and left out of the
  bundle. The NVL chunk (about 520 KB gzip) loads only when a graph view is
  about to show. The site build now also expands the first node of every
  graph result through the WASM engine and fails if that errors or misses
  a recorded relationship.
- **`dbms.components` `Neo4j Kernel` row reports `2026.10-cypherlite`
  (ADR-0152, LABS-150).** browser.neo4j.io's neo4j-sdk minimum-version gate
  (`>= 4.4.0`, loose-coerced semver) reads only the `Neo4j Kernel` row's
  `versions[0]` and throws on the honest `0.9.x` workspace version,
  blocking the Database information sidebar outright. The row now reports
  the ADR-0152 policy constant `2026.10-cypherlite` — the Neo4j calver
  month the Browser compat surface was verified against, plus the
  `-cypherlite` product suffix (Aura's `5.27-aura` convention), bumped
  deliberately rather than derived from the workspace version. The
  `CypherLite` row keeps the honest workspace version, the `Cypher` row is
  unchanged, and the Bolt HELLO server agent stays
  `CypherLite/<workspace version>` (the gate never reads it).

### Added

- **`db.schema.visualization()` (LABS-150).** The Neo4j-compatible schema
  graph procedure Browser/Bloom-style clients call for their sidebar is now
  implemented for real instead of failing with `ProcedureNotFound`:
  `CALL db.schema.visualization() YIELD nodes, relationships` returns one
  row whose `nodes` list holds one virtual node per stored label (Neo4j's
  `{name, indexes, constraints}` property shape, with `indexes`/
  `constraints` empty documented placeholders) and whose `relationships`
  list holds one virtual relationship per distinct `(startLabel, relType,
  endLabel)` triple, with multi-label endpoints expanded over every label
  combination. Virtual entities carry negative ids via the CIP-257
  machinery, so `startNode()`/`endNode()` resolve them and writes against
  them follow the CIP-257 virtual-entity rules; the empty graph yields
  zero rows. Listed by `SHOW PROCEDURES` like its `db.schema.*` siblings,
  inferred by scanning stored entities per ADR-0112.

## [0.10.0] — 2026-09-30

## [0.9.2] — 2026-09-30

### Added

- **Honest `dbms.showCurrentUser` via a per-connection principal
  (ADR-0273).** `cypherlite::Connection` gains
  `set_principal(Option<&str>)` and `principal() -> Option<&str>`: an
  optional, display-only per-connection identity that confers no
  authorization. The Bolt server sets it once after a HELLO validates
  under `--auth user:pw` (or `CYPHERLITE_AUTH=user:pw`), so
  `dbms.showCurrentUser()` reports the validated user as `username`
  instead of the ADR-0143 placeholder; the generated-token default takes
  the same path and reports `'cypherlite'`, now an honest read (that is
  the configured user). Under `--auth none` the unverified HELLO
  principal is never forwarded, so `username` stays the documented
  placeholder `'cypherlite'`, byte-identical to ADR-0143; non-Bolt
  surfaces (CLI, Rust, WASM, FFI) never set a principal and see the same
  placeholder row. `roles: ['admin']` and `flags: []` are unchanged, as
  are `dbms.clientConfig`, `dbms.listConfig` and
  `dbms.licenseAgreementDetails`.
- **Browser/Desktop-compat `dbms.*` procedures (ADR-0143).** The four
  remaining procedures neo4j-browser and Neo4j Desktop call during or after
  connect now answer from the engine with fixed rows, per the ADR-0142
  documented-placeholder precedent: `dbms.showCurrentUser()` returns
  `{username: 'cypherlite', roles: ['admin'], flags: []}` (`flags` honest,
  the rest documented placeholders — the engine carries no session
  identity); `dbms.clientConfig()` and `dbms.listConfig()` return the
  8-column Neo4j 5.x+ `ConfigResult` shape with one row per key of the
  default allowlist (`browser.allow_outgoing_connections`,
  `browser.credential_timeout`, `browser.post_connect_cmd`,
  `browser.remote_content_hostname_whitelist`,
  `browser.retain_connection_credentials`, `browser.retain_editor_history`,
  `client.allow_telemetry`, `dbms.security.auth_enabled`,
  `server.metrics.prefix`), with `dynamic: false`, `defaultValue` and
  `startupValue` equal to `value`, `explicitlySet: false`, and `validValues:
  ''` on every row — documented placeholders except the honest
  `server.metrics.prefix: 'cypherlite'` and `client.allow_telemetry:
  'false'`; and `dbms.licenseAgreementDetails()` returns the honest constant
  `{status: 'yes', daysLeftOnTrial: null, totalTrialDays: null}` (Apache-2.0,
  no license agreement and no trial) as defensive completeness — no current
  client calls it against a community-edition server. Resolves
  [LABS-148](https://linear.app/neo4j/issue/LABS-148) (consolidating
  LABS-140/141/142/143); together with ADR-0142 this closes the
  Browser/Desktop compatibility work stream.
- **Bolt over WebSocket and neo4j-browser support (ADR-0266).**
  `cypherlite-bolt` serves Bolt over WebSocket (RFC 6455) on the same
  `--bind`: a first byte of `0x47` routes to a WebSocket upgrade (`ws://`),
  and the TLS path re-sniffs the decrypted stream so `wss://` and `bolt+s://`
  share the port. Inbound binary payloads concatenate into the Bolt byte
  stream, each outbound Bolt write is one binary message, and a text frame
  closes with code 1003. The upgrade request is bounded at 8192 bytes, frames
  at 1 MiB, and reassembled messages at 16 MiB; a malformed upgrade gets
  `HTTP/1.1 400 Bad Request` and a rejected Origin `HTTP/1.1 403 Forbidden`.
  WebSocket serving is on by default (`--no-websocket` opts out, reported as
  `ws=on|off` in the startup banner) with an auth-dependent Origin policy —
  any Origin with auth on, loopback Origins only with `--auth none` —
  overridable with the repeatable `--ws-allow-origin <ORIGIN>` (`*` allows
  any). neo4j-browser and the official JavaScript driver's browser build
  connect out of the box: the browser's default `neo4j://` scheme works
  because ROUTE now answers with a single-server self routing table
  (ADR-0269), and calls to unregistered procedures carry
  `Neo.ClientError.Procedure.ProcedureNotFound` (ADR-0138) so the browser's
  config-procedure fallback fires. An https-hosted Browser (e.g.
  browser.graphapp.io) dials `wss://` and needs a browser-trusted
  certificate on the server.
- **Bolt WebSocket hardening round (ADR-0139, ADR-0140, ADR-0141).** A
  review round on the WebSocket transport and Bolt layer: outbound
  backpressure bounds pong replies to a ping flood (at most one pong ever
  outstanding; a client that never reads stalls instead of growing an
  unbounded write buffer), and a Bolt session end now drives a
  timeout-bounded WebSocket close handshake so GOODBYE yields a server
  Close frame before EOF. The Origin check fails closed on a
  non-visible-ASCII `Origin` header, loopback parsing rejects userinfo,
  paths, non-numeric ports, and bracketed non-IPv6 hosts, and rejection
  logs carry only an escaped 128-character excerpt, never the raw Origin.
  A failed HELLO waits a fixed 1000 ms before the
  `Neo.ClientError.Security.Unauthorized` FAILURE on every transport,
  bounding online guessing of a preset credential (ADR-0139). ROUTE
  advertises the routing-context address the client dialed, falling back
  to the connection's local socket address, so Docker port mapping and
  NAT deployments get a routable address (ADR-0140). The handshake
  timeout is now one accept-time deadline spanning all five negotiation
  phases instead of a fresh per-phase budget, discharging into the idle
  timeout after the version exchange (ADR-0141).
- **Cookbook (ADR-0130).** Task-shaped recipes in
  [`docs/cookbook/`](docs/cookbook/): entity resolution, nearby places,
  dependency blast radius, hybrid recall (vector + full-text + RRF),
  long-term agent memory, Stack Overflow tag co-occurrence, an agent
  trajectory timeline (latest-N reads), bi-temporal decisions, tool-call
  provenance, a context graph for a codebase, and rehearsing a migration
  against a golden graph. Each recipe
  is one markdown file (ADR-0132) with runnable `cypher cell=<id>` steps,
  `json data=<name>` blocks for their parameters, and each step's recorded
  result as a `table` block or, for nodes and paths, a
  [gram](https://github.com/gram-data/tree-sitter-gram) block.
  `cargo test -p cypherlite-cookbook` runs every recipe natively and the
  site build runs it through the WASM bundle; both must match the
  recording, in always-on CI jobs (`make cookbook`). One Rust parser and one
  renderer (`cypherlite_formatter::recording`) serve the harness and the
  browser alike (ADR-0131, ADR-0132). The Pages
  deploy publishes the recipes at `/cypherlite/cookbook/`, built from the
  latest release tag, where every step runs in the browser (engine in a Web
  Worker, with Stop and Reset) and links into the playground. Pages link
  only to public targets while the repository is private, and the build
  fails on any link that does not resolve (ADR-0133). Every recipe draws
  its data model, and the longer ones their pipeline, as mermaid diagrams
  that GitHub renders and the site shows pre-rendered (ADR-0135); the site
  draws a data model as a graph, in the style and palette of Neo4j Bloom
  and NVL, which the result graph view now shares.
- **WASM script execution and rendering (ADR-0131).** `cypherlite-wasm`
  exports `executeScript(script, params)`, which runs a multi-statement
  script with the engine's own runner (`cypherlite::run_script_wire`, new,
  over `cypherlite::execute_wire`, new, which `execute` now is too),
  and `renderResult(envelopes)`, which renders a result as a cookbook
  recording (its text, table cells, graph, and summed counters); the bundle
  ships `isWasmTrap` in `trap.js`. WASM parameters now
  decode as the C ABI's do (`cypherlite::params_from_json`, new): a map with
  a `__type` key stays a map. `cypherlite_splitter::split_executable_statements`
  drops comment-only pieces. The documented WASM API examples run in CI
  (`surfaces/cypherlite-wasm/tests/examples.mjs`).
- **Playground scripts and cookbook links.** The playground runs
  multi-statement input, and opens a cookbook step from
  `#recipe=<slug>&cell=<id>` after the reader confirms, replaying the
  recipe's earlier steps and keeping the step's parameters.
- **Browser/Desktop-compat record shapes for `dbms.components`, SHOW
  DATABASES, and `dbms.info` (ADR-0142).**
  `dbms.components` gains the `edition` output and returns three rows:
  `{name: 'Neo4j Kernel', versions: [<engine version>], edition:
  'community'}` first — row order is load-bearing: Neo4j Desktop finds the
  version row *by name* while the public web Browser's version gate reads
  only the first record's `versions[0]` — then `{name: 'Cypher', versions:
  ['5', '25'], edition: 'community'}`, then `{name: 'CypherLite', versions:
  [<engine version>], edition: 'community'}`, identifying CypherLite to
  consumers that iterate all rows. Because the engine version is below
  Desktop's `4.4.0` gate, Desktop shows a non-fatal "not officially
  supported" warning on connect (accepted as honest output, ADR-0142). SHOW
  DATABASES rows carry concrete values instead of nulls: `access`
  `'read-write'`, `address` `'localhost:7687'`, `role` `'primary'`,
  `requestedStatus`/`currentStatus` `'online'`, `statusMessage` `''`,
  `writer` `true`, and `default`/`home` `true` for `neo4j` and `false` for
  `system`. These are fixed constants, not live status: CypherLite is a
  single-node embedded engine, so every namespace is always
  online/primary/read-write. `dbms.info()` answers Desktop's DBMS-info call
  with one row `{id: 'cypherlite', name: 'system', creationDate:
  '1970-01-01T00:00:00Z'}` (`creationDate` is a documented placeholder).
  Neo4j Desktop's embedded Browser now connects over the
  Bolt-over-WebSocket transport (ADR-0266): its version check finds the
  `Neo4j Kernel` row, its `name@address` instance id validates, and its
  record codec accepts the SHOW DATABASES rows. Closes
  [LABS-139](https://linear.app/neo4j/issue/LABS-139).
- **Bolt Path values encode as a PackStream structure (ADR-0272).** A
  `Path` returned over Bolt was stringified to a Cypher literal, so drivers
  could not reconstruct the graph pattern it described. It now encodes as a
  `Struct 0x50` (`nodes`, `relationships`, `sequence`) whose relationships
  are the three-field `UnboundRelationship` form (`0x72`), with a
  two-entries-per-hop `sequence` whose relationship index is negative for a
  reverse hop; the same encoding applies at any nesting depth inside a List
  or Map.
## [0.9.1] — 2026-09-28

## [0.9.0] — 2026-09-28

## [0.8.1] — 2026-09-28

### Changed

- **Fixed namespaces with Neo4j Community Edition database semantics
  (ADR-0116, ADR-0117, ADR-0118, ADR-0119).** Every store now has exactly
  three fixed namespaces — `neo4j` (the user data graph), `system` (no
  graph data) and `system_ontologies` (read-only) — replacing the
  metadata-only database theater. `CREATE [OR REPLACE] [COMPOSITE]
  DATABASE`, `DROP [COMPOSITE] DATABASE`, `ALTER DATABASE` other than
  `SET DEFAULT LANGUAGE CYPHER 5|25`, `START`/`STOP DATABASE`, the
  database-alias commands and `SHOW ALIASES FOR DATABASE` no longer record
  a row: they fail with
  `Unsupported administration command: <statement>` (GQLSTATUS 51N27).
  `USE` routes its statement to one of the fixed namespaces instead of
  being ignored: an unknown name fails with `Graph not found: <name>`
  (42N00) before execution, composite-only `graph.byName`/`graph.names`/
  `graph.propertiesByName` fail with 42N72, and mixed targets in one
  statement fail with 42NA5. Writes on `system_ontologies` fail with
  42N18, and data clauses on `system` fail with 42N17. `SHOW DATABASES`
  lists the three fixed rows (`neo4j` is `default` and `home`;
  `system_ontologies` is read-only) and `db.info()`/
  `db.nameFromElementId()` report the statement's namespace; element ids
  gained an optional namespace segment (`4:cypherlite.system_ontologies:n5`,
  ADR-0116 §7). New namespace-selection APIs on every surface: Rust
  `Database::connect_namespace`/`Connection::set_namespace`, C
  `cypherlite_connect_namespace`/`cypherlite_conn_set_namespace` (and the
  JS/Java/Go clients wrapping them), Python connection namespace
  selection, the Bolt `RUN`/`BEGIN` `db` extra, and the CLI `--namespace`
  flag. The CIP-eval frozen set drops the 24 scenarios that need
  Enterprise multi-database behavior (ADR-0119): CIP-170 is now 9/30,
  CIP-141 34/38, overall 5707/5745.
- **WAL recovery rejects undecodable records and checks the store version
  first (ADR-0114).** A complete WAL frame whose CRC matches but whose payload
  does not decode now fails WAL open, replay and CDC `records_after` with
  `Error::Storage` in every segment and position; the error names the segment
  and says the record `may have been written by a newer CypherLite`. Only a
  CRC failure on the final complete frame of the active segment (and
  incomplete frames on any segment) stays a tolerated tail; recovery applies
  tolerated-tail trims only after every segment has been scanned, and repairs
  the highest segment's short magic only after every earlier segment has
  validated. `Database::open` now
  also validates the snapshot's format marker before WAL recovery — Parquet
  manifest `format_version` 1 to 3; JSON `graph.json` `value_encoding` absent
  or 2 with `read_only` false — failing with `unsupported snapshot version N`,
  `unsupported graph value encoding: N` or `read-only snapshot not supported`
  and leaving every WAL segment unchanged.

- **macOS CLI binaries are Developer-ID signed and Apple-notarized
  (ADR-0123).** The release workflow's darwin legs sign each `cypherlite-darwin-*`
  binary with a Developer ID Application identity and submit it to Apple's
  notary service before upload, so Gatekeeper accepts a downloaded binary
  without the System Settings bypass. The signing and notarization are
  performed by one shared composite action,
  `.github/actions/macos-notarize/action.yml`, proven end-to-end by that
  action's self-verifying, fail-closed pipeline. The FFI dylib and Python
  wheels remain unsigned.

## [0.8.0] — 2026-09-27

## [0.7.4] — 2026-09-27

### Changed

- **`cypherlite upgrade` refreshes the installed skill bundle by ownership
  (ADR-0261).** After a successful self-replace, each detected agent's
  `<skills dir>/cypherlite` is classified Missing, Owned
  (`.cypherlite-installed` marker), Legacy (no marker, but `SKILL.md`
  frontmatter declares `name: cypherlite` and contains a `version:` line), or
  Foreign (neither; a `name: cypherlite` `SKILL.md` with no `version:` line is
  Foreign). An installed bundle (Owned ∪ Legacy) is
  updated automatically with no prompt — a plain `skill install`, no `--force`.
  Only when nothing is installed anywhere does an interactive upgrade ask
  `Install the cypherlite skill? [Y/n]` (empty means yes); `--yes` installs
  without asking and a non-interactive run without `--yes` prints
  `run: cypherlite skill install`. A Foreign directory is never overwritten: it
  is reported with
  `note: {path} is not a recognizable cypherlite skill (no .cypherlite-installed marker, and SKILL.md does not declare name: cypherlite with a version line); leaving it untouched`
  and pointed at `cypherlite skill install --force`, and when it
  coexists with installed or missing agents the refresh falls back to one
  `cypherlite skill install --agent <name>` per non-foreign agent so a forced
  clobber never reaches it. The upgrade exit code stays 0 on every post-replace
  path. No new CLI flag is added.
- **Standalone `cypherlite skill install` adopts a recognizable cypherlite
  bundle (ADR-0262).** Without `--force`, `skill install` now replaces a
  destination that is missing, marker-owned (`.cypherlite-installed` regular
  file), or a recognizable cypherlite bundle — no marker, but `SKILL.md`
  frontmatter declares `name: cypherlite` and contains a `version:` line. Any
  other existing directory, and any symlinked destination, is still refused
  (exit 1, untouched) until `--force`. This lets the post-upgrade refresh spawn
  a plain `skill install` and re-validate ownership in the process that
  performs the replacement, closing a TOCTOU window. `skill remove` is
  unchanged: it still refuses an unmarked directory without `--force`.

## [0.7.3] — 2026-09-26

### Added

- **Optional Parquet snapshot codec: `parquet` Cargo feature (ADR-0105).**
  `cypherlite` and `cypherlite-storage` gain a `parquet` feature, on by
  default, that compiles the Parquet codec and its Arrow/Parquet
  dependencies. Default builds behave as before. With
  `default-features = false`, new stores use the JSON codec, and a Parquet
  store or an explicit Parquet request (`OpenOptions.snapshot_codec`,
  `CYPHERLITE_SNAPSHOT_FORMAT`) fails at open with `parquet snapshot codec
  is not compiled into this build (enable the "parquet" feature)`; there is
  no fallback to JSON. A manifest without a `codec` field still means
  Parquet. `cypherlite_storage::window_width` and
  `DEFAULT_SHARD_TARGET_BYTES` exist only with the feature. Inside the
  workspace, the `cypherlite` and `cypherlite-storage` workspace
  dependencies set `default-features = false`; members that need Parquet
  enable it explicitly (`cypherlite-wasm` does not). See
  [Cargo features](docs/api/rust/spec.md#cargo-features).
- **`SnapshotCodecKind::is_compiled` and `SnapshotCodecKind::compiled`
  (ADR-0105).** Report at runtime which snapshot codecs the build has:
  `is_compiled(self) -> bool` (`const fn`) and
  `compiled() -> &'static [SnapshotCodecKind]`, the new-store default
  first. `SnapshotCodecKind::default()` is `compiled()[0]`.

### Changed

- **`:memory:` automatic checkpoints skip the snapshot dump (ADR-0105).**
  WAL-rotation and close checkpoints of `:memory:` databases no longer
  encode a snapshot nobody can read back; they only recycle the WAL (still
  bounded by CDC slots) and GC old versions, on every target. An explicit
  `Database::checkpoint()` still writes a snapshot, so
  `snapshot_data_bytes()` on `:memory:` stays `0` until one runs. Durable
  stores are unchanged.
- **`cypherlite_storage::memory_store_with_codec` returns
  `Result<StoreTarget>` (ADR-0105).** It rejects a codec that is not
  compiled into the build with the same `Storage` error as `Database::open`,
  instead of failing later.
- **CLI `parquet` feature; `pull` and the `agent` bundle no longer need
  Parquet (ADR-0105, ADR-0106).** `cypherlite-cli` gains a `parquet`
  feature in the default `full` bundle, so release binaries are unchanged.
  Because JSON snapshots now persist pull identity (ADR-0106), neither the
  `pull` feature nor `cypherlite-load` enables Parquet, and the `agent`
  bundle builds without Arrow/Parquet (137 → 90 normal crates). `agent` and
  `minimal` builds create JSON stores, list `(json)` for
  `--snapshot-format` in `--help`, and refuse Parquet stores at open. See
  [Build bundles](docs/api/cli/spec.md#build-bundles).

## [0.7.2] — 2026-09-25

## [0.7.1] — 2026-09-25

## [0.7.0] — 2026-09-24

### Added

- **WASM distribution reinstated (ADR-0084 / ADR-0085).** `crates/cypherlite-wasm`
  ships as a `wasm-pack --target web` package built from a shared
  [`scripts/build-wasm.sh`](scripts/build-wasm.sh) (invokable via
  `make build-wasm` or `npm run build:wasm`). The wasm32 engine build is
  `:memory:` only, uses the JSON snapshot codec, and runs single-threaded
  (`web-time::Instant` in place of `std::time::Instant`, no thread spawn).
  Public surface docs at [`docs/api/wasm/`](docs/api/wasm/); example
  consumer at [`examples/wasm-playground/`](examples/wasm-playground/) (a
  Vite + React app). A `wasm` CI job runs `cargo check` on
  `wasm32-unknown-unknown` for `cypherlite-storage` and `cypherlite-wasm`
  plus `wasm-pack build --release --target web` — failure blocks the
  merge (the reinstatement clause of ADR-0065).

## [0.6.1] — 2026-09-22

## [0.6.0] — 2026-09-21

### Added

- **Interactive CLI line editing (ADR-0073 / LABS-59).** On a TTY the
  `cypher>` prompt uses rustyline for GNU readline-style editing (arrow
  keys, Home/End, emacs bindings) and in-session history. Piped stdin and
  one-shot queries are unchanged. Ctrl-C cancels the pending buffer;
  there is no tab completion or history file.

## [0.5.0] — 2026-09-20

### Added

- **CLI self-upgrade command: `cypherlite upgrade` / `cypherlite update` (ADR-0069).**
  A positional lifecycle command that checks the latest GitHub release, downloads and
  checksum-verifies the platform binary against `SHA256SUMS`, and atomically replaces the
  running binary (macOS/Linux only; Windows keeps the manual `.exe` download). `--check`
  is a read-only current-vs-latest report; `--yes` auto-accepts the post-upgrade
  skill-bundle refresh (non-interactive runs without it print a
  `run: cypherlite skill install` hint). Honors `GITHUB_TOKEN` for release downloads
  while the repository is internal.

### Removed

- **Breaking:** the `--skill`/`-skill` and `--agent`/`-agent` flags are removed, replaced
  by the positional command `cypherlite skill install|remove [--agent NAME]` (ADR-0069).
  The removed spellings never shipped in a release, so they hit the generic unknown-flag
  error with no migration hints. Database paths named `skill`, `upgrade`, or `update`
  stay reachable via a `./` prefix or after `--`.

## [0.4.0] — 2026-09-19

### Added

- **Manual release workflow (ADR-0067).** Releases are dispatched manually
  from the Release workflow (`gh workflow run release.yml -f bump=...` or
  Actions → Release → Run workflow) with a `patch`/`minor`/`major` bump
  choice. A run bumps versions across the Cargo workspace and the Python
  package, rotates this changelog, builds all CLI/FFI/Python artifacts, tags
  `vX.Y.Z` after green builds, and creates the GitHub release with
  `SHA256SUMS` and generated notes (PyPI publishing is deferred for now —
  the Python wheels and sdist ship as GitHub release assets only). The first
  release is `v0.4.0`;
  see `RELEASING.md` for the dispatch and failure-recovery runbook.
- **Neo4j CIP Cypher coverage** — an evaluation suite (`cypherlite-cip-eval`, the 14th crate)
  derived from 164 Neo4j Cypher Improvement Process documents. 5629 Gherkin scenarios (verified
  against a live Neo4j under Cypher 25), gated by a ratchet baseline; **5615/5629 (99.8%) passing**.
  Scope triage in `testdata/cip-eval/MANIFEST.md`; residual and limitations in
  `testdata/cip-eval/SWEEP-FOLLOWUP.md`. New `make cip-eval` / `make cip-eval-baseline` targets.
- **GQL / GPM Cypher features beyond openCypher**: label expressions (`&` `|` `!` `%`, `IS`),
  inline pattern `WHERE`, quantified path patterns, path modes/selectors, dynamic labels/types/
  property keys (`$(expr)`, `n[key]`), the `LABELED` predicate.
- **Type system**: `IS ::` type-predicate expressions, `valueType()`, Cypher type names, property
  **type** constraints, `NOT NULL` shorthands, Infinity/NaN literals.
- **New clauses**: `LET`, `FILTER`, `NEXT`, `FINISH`, `INSERT`, conditional `WHEN … THEN`,
  `UNION DISTINCT`, `RETURN ALL`, `NODETACH DELETE`, standalone page statement, `CALL (x,y) {}`
  scope clause, `COUNT {}`/`COLLECT {}` subquery expressions, read-after-write (update-boundary removal).
- **First-class `VECTOR<type>(dim)` value type** with vector functions, similarity, index DDL, and
  ANN search with single-stage filtering.
- **Graph-type sublanguage**: `CREATE GRAPH TYPE`, `ALTER CURRENT GRAPH TYPE`, `SHOW CURRENT GRAPH
  TYPE`, canonical form, conformance, and node/relationship visualization.
- **`CALL {} IN TRANSACTIONS`** — `OF n ROWS` batching, `REPORT STATUS`, `ON ERROR BREAK|CONTINUE|
  FAIL|RETRY`, `CONCURRENT TRANSACTIONS` (adapted to sequential batching), with per-batch rollback.
- **Per-statement Cypher version selection** (`CYPHER 5` / `CYPHER 25`).
- **Temporal `format()` / `parse()`**, GQL string functions (`trim`, `normalize`, `char_length`),
  IEEE-conformant + hyperbolic math, `||` concatenation, `allReduce()`, `NULLIF`, aggregate
  set-quantifiers, and many GQL function aliases; graduated APOC text functions.
- **Canonical JSON wire encoding** (`cypherlite_model::wire`) — one self-describing, documented
  encoding for values (structured nodes/relationships/paths, ISO-8601 temporals, coordinate-key
  points with a `srid`/`crs` marker, GeoJSON-style geometries, and structured `{type,dim,values}`
  vectors — no value stringified via `Display`), the 11 mutation counters (snake_case, zeros
  included), and the response envelope (`rows`/`columns`/`stats`/`error`). Shared golden fixtures
  (`cypherlite_model::wire::golden`) pin the contract from every surface's test suite. The
  C ABI / FFI, CLI, and Python surfaces migrate onto it across the rest of this release; when they
  do, the wire changes are breaking (acceptable pre-1.0): the response row container key
  `data` → `rows`; the stat names `indexes_created`/`indexes_dropped` →
  `indexes_added`/`indexes_removed` (and the `constraints_*` equivalents); and vectors/temporals
  that were previously emitted as `Display` strings become structured/ISO values.
- Single-level transactions for memory and JSON storage. The Swift binding exposes `withTransaction`, which
  durably flushes on successful return and rolls back execution and ordinary persistence failures. Nested transactions
  are rejected, manual flush/reset cannot bypass an active boundary, and dropping an active JSON store restores its
  pre-transaction snapshot. If compensating restoration itself fails, a pre-publication sentinel and typed error poison
  the store instead of reporting a false rollback. Lance intentionally reports the capability as unsupported until
  version consistency is resolved. (Superseded: the Swift surface is withdrawn — see Removed, ADR-0065.)
- A repository-root Swift package manifest, while preserving `bindings/swift/Package.swift` for existing local
  consumers. `scripts/check-swift-package-parity.sh` (local tooling) checks both manifests for release metadata, products, dependencies, and binary/source/test-path parity.
  XCFramework archives now canonicalize `xcodebuild`'s library ordering and normalize ZIP-observable metadata, while
  the pre-tag preparation gate proves two independent
  clean-target builds are byte-identical before embedding their checksum in both manifests. Official tag and release
  workflows now fail closed outside `neo4j-labs/cypherlite`, the repository addressed by both binary manifests.
  (Superseded: the Swift surface is withdrawn — see Removed, ADR-0065.)

### Changed

- **Release builds for Intel macOS moved to `macos-15-intel` runners.** GitHub
  retired the `macos-13` runner label, so the Release workflow now builds the
  `x86_64-apple-darwin` CLI/FFI artifacts on `macos-15-intel` runners; the
  build targets and produced artifacts are otherwise unchanged.
- **MemoryDB branding retired across live surfaces (ADR-0066).** The archived
  predecessor's name is removed from every surface that ships or gates the
  product: the npm package is `cypherlite`, the Go module is
  `github.com/neo4j-labs/cypherlite/clients/go`, the Java client moves to
  groupId/package `com.cypherlite` (`MemoryDbException` ->
  `CypherLiteException`), the CIP scoreboard is `baselines/cip-eval.json`, the
  CI/test env vars are `CIP_EVAL_*`, and the diagnostic `CYPHERLITE_PARSER`
  value is `planner-ir`. The name survives only in immutable ADRs, released
  changelog sections, and frozen research notes, where it is history.
- **Python and Swift now use `cypherlite::Database`.** The PyO3 package and the
  stable Swift-facing `cypherlite_*` ABI no longer execute through
  `Engine`/`GraphStore`. Public binding APIs and canonical wire values remain
  stable. Persistent paths now name CypherLite directories (WAL + Parquet or
  GraphData JSON snapshot); legacy single-file JSON is an explicit migration.
  Binding `flush()` checkpoints—the WAL commit remains the durability boundary.
  (Superseded: the Swift surface is withdrawn — see Removed, ADR-0065.)
- **WASM is `cypherlite::Database` `:memory:` only (ADR-0051).** `cypherlite-wasm` no
  longer wraps GraphStore or IndexedDB. `CypherLiteDb.create()` accepts omitted /
  `memory://` / `:memory:`; `idb://` and file paths error. `flush()` is a no-op.
  `exportJson` / `importJson` are GraphData (ADR-0050) for localStorage portability.
  The playground has no Storage switch. A headless-Chrome smoke of `:memory:`
  (`RETURN 1`, CREATE/MATCH) runs locally via `make test-wasm-browser`. The
  wasm32 build omits Arrow/Parquet snapshot
  codecs so the playground binary stays within the size budget. (Superseded:
  the WASM surface is withdrawn — see Removed, ADR-0065.)
- **JSONL server (`--server`) migrated onto the canonical wire encoding** (`cypherlite_model::wire`).
  Breaking wire changes
  (acceptable pre-1.0): the response row container key `data` → `rows`; the mutation-stat keys
  `indexes_created`/`indexes_dropped`/`constraints_created`/`constraints_dropped` →
  `indexes_added`/`indexes_removed`/`constraints_added`/`constraints_removed` (all 11 counters,
  snake_case, zeros always included); vectors and temporals now emit structured/ISO values instead of
  `Display` strings; and points now carry an explicit `srid`/`crs` marker. **All** responses — query
  results, query errors, protocol errors, and the `close` acknowledgement — now share one uniform
  envelope `{columns, rows, stats, error}` (the `close` reply changed from `{"ok":true}` to this
  envelope, where `error: null` is the durability acknowledgement). New behaviour: an optional
  client-supplied request `id` is echoed in every response (request/response correlation for
  pipelining); a missing/non-string `query` field and a non-object `params` value are now reported as
  protocol errors rather than executing the empty string or silently dropping the parameters.
  (Superseded: the JSONL server surface is withdrawn — ADR-0052.)
- **openCypher TCK harness hardened and its conformance claim restated.** The runner now runs strict
  by default: typed error **class** and **phase** matching for `should be raised` steps, exhaustive
  side-effect assertions (bare `no side effects` and every unlisted counter are really checked),
  distinct Int/Float comparison, and hard failure on unknown steps. The workspace's two former TCK
  harnesses were merged into this single strict runner. Under it **3896 of 3897 pinned,
  cucumber-expanded scenarios pass** — 0 fail; the remaining scenario is a documented skip
  (the invalid-Unicode-hyphen lexing detail). The run is now an enforced CI floor:
  `test_tck_full_summary` asserts `failed == 0`, zero lenient error fallbacks, the pinned 3897
  denominator, exact set equality with the checked-in passing-id allowlist
  (`crates/cypherlite-tck/src/allowlist.txt`), and that the reported skip set matches the documented
  `EXPECTED_TCK_SKIPS` list. (`cypherlite-cip-eval` deliberately keeps
  its own lenient semantics; its 5615/5629 ratchet baseline is unchanged.)

### Performance

- **Bulk `CREATE` no longer maintains string indexes eagerly (ADR-0063).**
  Prefix/trigram postings for `CONTAINS` / `STARTS WITH` are now built lazily
  per `(label, property)` pair on first seek and caught up incrementally
  (per-pair dirty sets) on later writes; the residual predicate re-check is
  unchanged, so results are identical. A 1M-node + 1M-rel single-statement
  create with a string property drops from ~46 s to ~9 s on the reference
  host (commit-phase `apply_redo` 39.5 s → 4.2 s), and peak RSS from ~19 GB
  to ~14 GB. Label/equality postings key on interned ids and bulk installs
  index/charge from compact parts; the WAL append no longer deep-clones the
  redo ops on the file path (on-disk bytes unchanged).

### Removed

- **WASM and Swift surfaces withdrawn (ADR-0065).** `crates/cypherlite-wasm`
  (including its persist stack: `src/persist/`, `OpenDb`, `PersistBackend`,
  IndexedDB deps, and the `browser_idb` / native JsonStore wasm tests) and
  the docs playground/homepage live terminal (`docs/src/pages/playground.tsx`,
  `CypherTerminal.tsx`, the `cypherlite-wasm.ts` / `terminalSession.ts` libs, the
  checked-in `docs/static/wasm/` bundle, and the wasm guides) are deleted, as are
  the repository-root `Package.swift`, `bindings/swift`, `examples/ios-snotel`,
  and the XCFramework / Swift release scripts. `cypherlite-ffi` remains the C ABI
  for the JS/Java/Go clients; reviving either surface requires a new ADR.
- **WASM-era public Rust API removed (ADR-0065).** `Database::from_memory_graph`
  and `Database::export_graph_json` (WASM export/import plumbing) are gone, along
  with the `graph_json_dump` / `graph_json_load` re-exports from `cypherlite`;
  the codec itself still lives in `cypherlite-storage`.

### Fixed

- **`EXPLAIN`/`PROFILE` statement prefixes report the plan again
  ([#14](https://github.com/neo4j-labs/cypherlite/issues/14)).** On the merged engine,
  `EXPLAIN <stmt>` lowered to a synthetic `RETURN NULL AS _explain LIMIT 0` stub, so it printed an
  empty `_explain` table, and `PROFILE` was parsed but executed as a plain query with no plan or
  statistics — both failed silently. `EXPLAIN` now plans the wrapped statement (parameters,
  `CYPHER <version>` preambles, `UNION` parts, every stage of a `NEXT` chain, and `CALL` subqueries
  included), renders the logical plan as rows of a single `plan` column, and executes nothing
  (no writes, no procedures, no `LOAD CSV`); a statement that cannot be planned fails with the
  planner's typed error instead of an empty table. `PROFILE` executes the statement and returns its
  rows plus `_profile_plan`, `_profile_time`, `_profile_rows`, and `_profile_mutations` columns.
- **`=`/`<` now use the exact integer↔float comparison on every path.** `Value::eq_non_null` and
  `Value::compare` compared `Int`/`Float` pairs through a lossy `(i as f64)` cast that rounds away low
  bits for `|i| > 2^53`, so `9007199254740993 = 9007199254740992.0` wrongly held in `WHERE` residuals,
  `IN`, pattern post-filters, and MERGE — while `DISTINCT`/`GROUP BY`/`UNION` keys and the property
  index (exact since the property-index key normalization) correctly kept the two apart, so an index's
  presence could change a MATCH/MERGE answer for a large-magnitude integer paired with a nearby float.
  Equality, ordering, grouping keys, and index keys now all route through one exact rule
  (`cypherlite_model::canonical_key`'s `eq_i64_f64`/`cmp_i64_f64`/`f64_exact_i64`, ground-truthed
  against live Neo4j): `9007199254740993 = 9007199254740992.0` is `false` and
  `9007199254740993 > 9007199254740992.0` is `true` on every path, and vector `=` compares
  elementwise-numerically exactly like `DISTINCT` does. The dead `CanonicalKey`/`Norm`
  machinery in `cypherlite-model` (its storage-index consumer was removed earlier in this branch) was
  slimmed to just those shared exact-comparison helpers.
- **Write-only statements now emit an empty `rows` array on every surface.** A `CREATE`/`MERGE`/`SET`/…
  without `RETURN` previously produced `rows: []` from the JSONL server but `rows: [{}]` (one empty
  object) from the C FFI and WASM surfaces, because only the server guarded the empty-columns case.
  All three now assemble the row container through one shared `wire::rows_for_result` helper (write-only
  → `[]`), pinned by a shared `wire::golden::envelope_write_only` fixture asserted from the CLI
  (`crates/cypherlite-cli/tests/wire_golden.rs`), FFI (`crates/cypherlite-ffi/src/lib.rs`), and Arrow
  (`crates/cypherlite-arrow/tests/golden_schema.rs`) test suites so the shape cannot drift again.
  (Superseded in part: the JSONL server and WASM surfaces are withdrawn — ADR-0052, ADR-0065.)
- **Arrow round-trip (`execute_arrow`) no longer demotes nested graph values.** A `List<Node>` /
  `List<Relationship>`, a node/relationship nested in a `Map`, and a `Path` column now decode back to
  their `Node`/`Relationship`/`Path` variants. The self-describing Utf8 fallback columns carry canonical
  wire JSON and are now decoded by a wire-aware decoder (mirroring the former Swift shape-sniffer) instead of
  the `__type`-tagged persistence decoder, which silently produced `List<Map>` / a flat `List`.
- 11 pre-existing openCypher TCK DELETE side-effect failures — every DELETE side-effect scenario now passes.
- `DISTINCT` was silently ignored by aggregates other than `count()`.
- `ORDER BY` on an expression containing a RETURN alias (e.g. `names[0]`) sorted on null.
- Unique/key constraints and the current graph-type model are now persisted and re-enforced when
  a JSON-backed database is reopened.
- `RETURN *` / `WITH *` now report real column names instead of a literal `"*"`.
- JSON persistence now uses Windows' atomic replace-and-write-through operation when publishing an updated database.
  Repeated dirty commits no longer fail merely because the destination file already exists, and a cross-platform
  regression test reopens the database after two successive transactions.

## [0.3.0-alpha] — 2026-07-17 — Rust workspace

First release of the Rust implementation. CypherLite is now a pure-Rust,
13-crate Cargo workspace; the original Go implementation has been removed (its
final state is preserved at the [`v0.2.0-go-final`][go-final] tag). Databases
created by earlier versions keep opening — the JSON storage format is unchanged.

### Added

- **13-crate Rust workspace** — `cypherlite-{cli,repl,engine,parser,model,storage,lance,arrow,formatter,python,ffi,wasm,tck}`.
- **Native Lance columnar backend** (opt-in `--features lance`) — `GraphStore` + `VersionedStore` with MVCC time-travel (`.versions`, `.checkout`, `.compact`) and native vector search.
- **Arrow interop** — `CypherValue` ↔ `RecordBatch` conversion, GeoArrow WKB encoding, vectorized aggregation, and Arrow IPC output (`execute_arrow()`).
- **Three-tier vector search** — Lance native index → HNSW → brute-force cosine fallback; `vector()` type, `vector.*` functions, and `db.index.vector.queryNodes()`.
- **Geospatial** — WGS-84/Cartesian points (incl. 3D), geometry constructors and multi-geometry types, `spatial.*` predicates and measurements, R-tree `POINT INDEX`, WKT/GeoJSON conversion.
- **Python package** (`cypherlite`, PyO3 + maturin) — sqlite3-style `connect()`/`Connection`/`Cursor` plus the native `Database` API with zero-copy Arrow IPC.
- **Swift / iOS bindings** — `cypherlite-ffi` C ABI wrapped by a Swift package, distributed as an XCFramework for macOS and iOS.
- **WebAssembly target** — `cypherlite-wasm` powering the interactive docs playground.
- **CLI shell** — syntax highlighting, tab completion, persistent history, SQLite-style dot-commands, and output modes (table, CSV, JSON, JSONL, markdown, cypher).
- **JSONL server mode** (`--server`) — a line-delimited request/response protocol over stdin/stdout for out-of-process hosts. (The in-process Python and Swift bindings do not use it — they call the engine directly through PyO3 and the C FFI; they share only the wire encoding.)
- **Docusaurus documentation site** (Diátaxis: tutorials, guides, reference, explanation) with the WASM playground.

### Cypher language

- **openCypher TCK conformance** — 3877 of 3880 scenarios pass (0 fail; 3 documented skips) under a strict harness, run from the shared feature files in `testdata/tck/`.
- Reading (`MATCH`, `OPTIONAL MATCH`, `WHERE`, `RETURN`, `WITH`, `UNWIND`, `UNION`/`UNION ALL`), writing (`CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE`/`DETACH DELETE`, `FOREACH`), and advanced constructs (`CALL {}` and `EXISTS {}` subqueries, `CALL` procedures, list comprehensions, `reduce()`, `shortestPath`/`allShortestPaths`, named and variable-length paths, `EXPLAIN`/`PROFILE`, `LOAD CSV`, `CASE`, regex `=~`).
- Schema: `CREATE`/`DROP INDEX` (secondary, vector, point) and `CREATE`/`DROP CONSTRAINT` (unique, not null, node key).
- 60+ built-in functions across scalar, string, math, trigonometric, list, temporal, type-conversion, vector, and geospatial categories.

### Changed

- Repository root is now the Rust workspace; all public surfaces (CLI, docs, examples, playground) run on the Rust implementation.
- Python package rebuilt on the native module (was a subprocess wrapper) — queries execute in-process.

### Removed

- The Go implementation and its build tooling. Preserved at [`v0.2.0-go-final`][go-final].

[Unreleased]: https://github.com/neo4j-labs/cypherlite/compare/v0.13.0...HEAD
[0.13.0]: https://github.com/neo4j-labs/cypherlite/compare/v0.12.9...v0.13.0
[0.12.9]: https://github.com/neo4j-labs/cypherlite/compare/v0.12.8...v0.12.9
[0.12.8]: https://github.com/neo4j-labs/cypherlite/compare/v0.12.7...v0.12.8
[0.12.7]: https://github.com/neo4j-labs/cypherlite/compare/v0.12.6...v0.12.7
[0.12.6]: https://github.com/neo4j-labs/cypherlite/compare/v0.12.5...v0.12.6
[0.12.5]: https://github.com/neo4j-labs/cypherlite/compare/v0.12.4...v0.12.5
[0.12.4]: https://github.com/neo4j-labs/cypherlite/compare/v0.12.3...v0.12.4
[0.12.3]: https://github.com/neo4j-labs/cypherlite/compare/v0.12.2...v0.12.3
[0.12.2]: https://github.com/neo4j-labs/cypherlite/compare/v0.12.1...v0.12.2
[0.12.1]: https://github.com/neo4j-labs/cypherlite/compare/v0.12.0...v0.12.1
[0.12.0]: https://github.com/neo4j-labs/cypherlite/compare/v0.11.8...v0.12.0
[0.11.8]: https://github.com/neo4j-labs/cypherlite/compare/v0.11.7...v0.11.8
[0.11.7]: https://github.com/neo4j-labs/cypherlite/compare/v0.11.6...v0.11.7
[0.11.6]: https://github.com/neo4j-labs/cypherlite/compare/v0.11.5...v0.11.6
[0.11.5]: https://github.com/neo4j-labs/cypherlite/compare/v0.11.4...v0.11.5
[0.11.4]: https://github.com/neo4j-labs/cypherlite/compare/v0.11.3...v0.11.4
[0.11.3]: https://github.com/neo4j-labs/cypherlite/compare/v0.11.2...v0.11.3
[0.11.2]: https://github.com/neo4j-labs/cypherlite/compare/v0.11.1...v0.11.2
[0.11.1]: https://github.com/neo4j-labs/cypherlite/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/neo4j-labs/cypherlite/compare/v0.10.1...v0.11.0
[0.10.1]: https://github.com/neo4j-labs/cypherlite/compare/v0.10.0...v0.10.1
[0.10.0]: https://github.com/neo4j-labs/cypherlite/compare/v0.9.2...v0.10.0
[0.9.2]: https://github.com/neo4j-labs/cypherlite/compare/v0.9.1...v0.9.2
[0.9.1]: https://github.com/neo4j-labs/cypherlite/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/neo4j-labs/cypherlite/compare/v0.8.1...v0.9.0
[0.8.1]: https://github.com/neo4j-labs/cypherlite/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/neo4j-labs/cypherlite/compare/v0.7.4...v0.8.0
[0.7.4]: https://github.com/neo4j-labs/cypherlite/compare/v0.7.3...v0.7.4
[0.7.3]: https://github.com/neo4j-labs/cypherlite/compare/v0.7.2...v0.7.3
[0.7.2]: https://github.com/neo4j-labs/cypherlite/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/neo4j-labs/cypherlite/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/neo4j-labs/cypherlite/compare/v0.6.1...v0.7.0
[0.6.1]: https://github.com/neo4j-labs/cypherlite/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/neo4j-labs/cypherlite/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/neo4j-labs/cypherlite/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/neo4j-labs/cypherlite/compare/v0.3.0-alpha...v0.4.0
[0.3.0-alpha]: https://github.com/neo4j-labs/cypherlite/releases/tag/v0.3.0-alpha
[go-final]: https://github.com/neo4j-labs/cypherlite/releases/tag/v0.2.0-go-final
