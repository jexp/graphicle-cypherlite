# CypherLite

> This is an experimental Neo4j Labs project and not part of Neo4j's supported product lineup. See the [Neo4j Labs disclaimer](./LABS_DISCLAIMER.txt).

**Graph data, zero infrastructure.**

CypherLite is a lightweight, embeddable graph database with a command-line interface inspired by SQLite. It uses the [Cypher](https://opencypher.org/) query language — the most widely adopted graph query language — and provides a zero-configuration, serverless experience for working with graph data.

CypherLite is written in Rust. The public crate is `cypherlite`
(`cypherlite::Database`: WAL + Parquet or JSON snapshots). It passes 3892 of
3892 pinned openCypher TCK scenarios (0 fail, 0 skips; one CIP-271-superseded
outline is removed from the pin, ADR-0188) and implements a
large slice of Neo4j's post-standard Cypher (GQL alignment).

📖 **Documentation: [neo4j-labs.github.io/cypherlite](https://neo4j-labs.github.io/cypherlite/)** — tutorials, guides, and the CLI / Cypher / Rust / C API references. Static site on the [`gh-pages`](https://github.com/neo4j-labs/cypherlite/tree/gh-pages) branch; deep references live in [`docs/`](docs/).

<p align="center">
  <img src="img/cypherlite.png" alt="Cypherlte" width="800" />
</p>

## Contributing

Development here moves through draft PRs: a draft runs only the fast gate (fmt, clippy, light tests, the automated reviews), you iterate on the review findings in ~10-minute loops, and a maintainer flips it to ready for review — which runs the full CI matrix before merge. Deferred review findings become `review-tasks` Linear issues that the nightly stellar job fixes and PRs automatically.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the details and [AGENTS.md](AGENTS.md) for the full agent contract.

## Install

### Install script (macOS/Linux)

The install script downloads the release binary for your platform (macOS and
Linux, amd64 and arm64), authenticates the release's `SHA256SUMS` manifest
against its OpenSSH SSHSIG signature using the stock `ssh-keygen` binary, checks
the binary against that verified manifest, and installs it to
`$HOME/.local/bin`:

```bash
curl -sSfL https://neo4j-labs.github.io/cypherlite/install.sh | GITHUB_TOKEN=$(gh auth token) bash
```

`GITHUB_TOKEN` is required only while the repository is internal — it
authenticates the download of the release assets. Get one with `gh auth token`
(or any token with access to the repository). Once the repository is public the
same line works without the token:

```bash
curl -sSfL https://neo4j-labs.github.io/cypherlite/install.sh | bash
```

The environment variable must be set on `bash` (the right-hand side of the
pipe); a prefix on `curl` alone never reaches the script.

Releases are signed with an Ed25519 key in the `cypherlite-release` namespace;
the public key is pinned in the script and never fetched from the release. The
installer **fails closed** — a missing or invalid signature, or a missing
`ssh-keygen` (`openssh-client`), aborts before anything is installed. See
[ADR-0115](docs/adr/0115-sshsig-release-signing.md).

The macOS binaries are also signed with a Developer ID Application certificate
and notarized by Apple, so Gatekeeper raises no warning when a downloaded binary
is first run ([ADR-0123](docs/adr/0123-macos-codesign-notarize.md)).

Prebuilt binaries for Linux, macOS, and Windows are published on the [releases page](https://github.com/neo4j-labs/cypherlite/releases/latest).

### Build from source

Requires the [Rust toolchain](https://rustup.rs/):

```bash
cargo build --release
./target/release/cypherlite --version
```

### Upgrading

An installed binary self-updates from GitHub releases. It authenticates the
release's `SHA256SUMS` signature against the public key pinned in the binary
**before** trusting any checksum, then verifies the download against the
verified manifest:

```bash
cypherlite upgrade          # download, verify, and replace (alias: update)
cypherlite upgrade --check  # report current vs latest; changes nothing
```

macOS/Linux only. On Windows, download the new `.exe` from the table above as
before. Like the install script, `upgrade` needs `GITHUB_TOKEN` while the
repository is internal: the direct download route answers 404 for an internal
repository, and the token is what authenticates the GitHub API asset-route
fallback. Once the repository is public the direct route serves the download,
and the same command works without a token.

After replacing the binary, `upgrade` refreshes the installed agent-skill
bundle (see [Agent skill](#agent-skill)): an installed bundle is updated
automatically with no prompt, a Foreign directory is never overwritten, and
`--yes` installs the bundle without asking when none is installed yet.

## Quick Start

```bash
# Help is the default when no statement is given
cypherlite

# Interactive shell
cypherlite query

# Open or create a database directory and start the shell
cypherlite query --database ./mydata

# One-shot query (:memory: when --database is omitted)
cypherlite "RETURN 1 + 2 AS sum"

# Print all flags
cypherlite --help

# Help for one command
cypherlite clone --help
```

## Example Session

```
$ cypherlite query --database ./friends

CypherLite v0.13.0
Type .help for help, .exit to quit.

cypher> CREATE (alice:Person {name: 'Alice', age: 30});
Nodes created: 1, Properties set: 2, Labels added: 1

cypher> CREATE (bob:Person {name: 'Bob', age: 28});
Nodes created: 1, Properties set: 2, Labels added: 1

cypher> MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})
   ...> CREATE (a)-[:FRIENDS_WITH {since: 2020}]->(b);
Relationships created: 1, Properties set: 1

cypher> MATCH (p:Person)-[r:FRIENDS_WITH]->(friend)
   ...> RETURN p.name, friend.name, r.since;
+--------+-------------+---------+
| p.name | friend.name | r.since |
+--------+-------------+---------+
| Alice  | Bob         | 2020    |
+--------+-------------+---------+
```

## Python

The `cypherlite` Python package wraps `cypherlite::Database` as a native extension
module (PyO3 + maturin) — queries execute in-process, with no subprocess or
server. Install from PyPI as `neo4j-cypherlite` (the name `cypherlite` is taken
by another project); the import is still `import cypherlite`. From a source
checkout (requires the Rust toolchain):

```bash
pip install neo4j-cypherlite          # pending the first tagged PyPI publish
make install-python                   # creates a venv and runs `maturin develop`
```

### sqlite3-style API

```python
import cypherlite

db = cypherlite.connect(":memory:")          # or a directory path for persistence

db.execute("CREATE (p:Person {name: $name, age: $age})", {"name": "Alice", "age": 30})
db.execute("CREATE (p:Person {name: $name, age: $age})", {"name": "Bob", "age": 28})
db.execute(
    "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) "
    "CREATE (a)-[:FRIENDS_WITH]->(b)"
)

cursor = db.execute(
    "MATCH (p:Person)-[:FRIENDS_WITH]->(friend) "
    "RETURN p.name, friend.name"
)
for row in cursor:
    print(f"{row['p.name']} -> {row['friend.name']}")

db.close()
```

### Native API with Arrow interop

```python
from cypherlite import Database

db = Database()                              # or Database("graph")
db.execute("CREATE (:Person {name: 'Alice', age: 30})")

result = db.execute("MATCH (n:Person) RETURN n.name AS name, n.age AS age")
print(result.data)  # [{'name': 'Alice', 'age': 30}]

# Arrow IPC for zero-copy transfer to Pandas / Polars
import pyarrow as pa
buf = db.execute_arrow("MATCH (n:Person) RETURN n.name, n.age")
df = pa.ipc.open_stream(buf).read_all().to_pandas()
```

See [`examples/python/python_demo.py`](examples/python/python_demo.py) for a full working example and the [Python Library Reference](docs/reference/python-library.md) for complete API docs.

## Docker (cypherlite server: Bolt + HTTP + MCP)

`make docker` builds the server image (multi-stage: a musl-native Alpine
Rust builder, a `FROM scratch` runtime with just the static binaries —
ADR-0128); since ADR-0249 the runtime is one `cypherlite serve` process
hosting Bolt (7687), the HTTP Query API v2 (7474), and MCP over
streamable HTTP (7475) on one store with one shared credential (the token
in `docker logs` unlocks all three). `make docker-run` runs it with all
three ports on localhost and a data volume:

```bash
make docker          # docker build -t cypherlite-bolt:<workspace version> -t cypherlite-bolt:latest .
make docker-run      # -p 7687:7687 -p 7474:7474 -p 7475:7475, data in ./target/docker-data:/data
```

Configuration is env-based (the engine's native vars pass straight
through). Seeding runs once, before the server starts — when the database
does not exist yet (`CYPHERLITE_FORCE_SEED=1` to reseed; `:memory:` always
seeds):

```bash
docker run -p 7687:7687 -p 7474:7474 -p 7475:7475 -v "$PWD/data:/data" \
  -e CYPHERLITE_LOGICAL_GRAPH_SIZE_LIMIT=2gb \
  -e CYPHERLITE_QUERY="UNWIND range(1,100) AS i CREATE (:Person {id: i})" \
  cypherlite-bolt:latest

# fetch-and-seed from a JSON API (needs a build with the `load` feature):
  -e CYPHERLITE_LOAD='value=https://api.stackexchange.com/2.3/questions?tagged=neo4j&site=stackoverflow' \
  -e CYPHERLITE_LOAD_QUERY='UNWIND $value.items AS q MERGE (:Question {id: q.question_id})' \

# mirror a Neo4j/Aura subgraph (Query API v2, ADR-0080):
  -e CYPHERLITE_PULL_FROM=/run/secrets/neo4j.env \
  -e CYPHERLITE_PULL_QUERY='MATCH (o:Organization)-[r]-() RETURN o, r LIMIT 500' \
```

Startup logs echo the effective configuration (uri, config source,
seed plan), misconfigurations fail loudly with the offending env var or
JSON path — `docker logs` is the debugging surface.

A test fleet — N serve processes in one container, every surface's port
stepping from its base, per-instance suffixed URIs. Unset `--http`/`--mcp`
bases derive from the bolt base so the stepped ranges never overlap (N=2:
bolt 7687-7688, http 7689-7690, mcp 7691-7692):

```bash
docker run -p 7687-7692:7687-7692 \
  -v "$PWD/data:/data" -e INSTANCES=2 \
  -e CYPHERLITE_QUERY="UNWIND range(1,50) AS i CREATE (:Person {id: i})" \
  cypherlite-bolt:latest
```

The container binds `--host 0.0.0.0` so the ports are reachable; `serve`
is plaintext-only and warns loudly about it (ADR-0248) — expected on this
dev/test image. The image carries `cypherlite` and `entrypoint` only
(ADR-0368 — the TLS-capable `--entrypoint /bin/cypherlite-bolt`
alternative is gone); for encrypted Bolt, front the container with a
TLS-terminating proxy.

`Ctrl-C` (attached runs) and `docker stop` stop the fleet — PID 1 handles
SIGINT/SIGTERM and kills every instance; `docker rm -f` also works, and the
fsync'd WAL makes any exit data-safe.

Container smoke suite: `make docker &&
python3 surfaces/cypherlite-bolt/tests/docker_smoke.py` (runs in CI).

The default `CMD` passes `--allow-plaintext` (a container must bind
`0.0.0.0` to be reachable at all, and the server's plaintext policy
(ADR-0109) refuses that otherwise) — this is a dev/test image; for TLS,
front it with a TLS-terminating proxy (serve has no TLS flags,
ADR-0368).

**Auth** (ADR-0126): the server authenticates by default — it generates a
token, prints it in the startup banner (`docker logs`), and persists it as a
`<uri>.auth-token` sibling (on the `/data` volume for the default URI) so
restarts keep it. Connect with it:

```bash
TOKEN=$(docker logs <container> 2>&1 | sed -n 's/.*generated token for user cypherlite: //p' | head -1)
python -c "from neo4j import GraphDatabase; print(GraphDatabase.driver('bolt://localhost:7687', auth=('cypherlite', '$TOKEN')).execute_query('RETURN 1 AS n')[0][0]['n'])"
```

Or preset it: `-e CYPHERLITE_AUTH=cypherlite:secret` (or `...=none` to
disable — publish ports only on trusted networks then).

**neo4j-browser** can connect to the container on `localhost:7687` with no
extra setup: Bolt-over-WebSocket is served on the same port by default
(ADR-0266), and an http-hosted Browser dials it over `ws://`. Use the auth
token from `docker logs` (above) as the password; user is `cypherlite`.


## Browser (WASM)

CypherLite also ships as an npm package so a page can
embed the engine and run Cypher without a server:

```bash
npm install @neo4j-labs/cypherlite-wasm
```

`new CypherliteDb()` opens an in-memory database; `await
CypherliteDb.open("opfs://<name>")` opens the durable OPFS store (ADR-0145),
which uses `FileSystemSyncAccessHandle` and therefore runs only in a
dedicated worker. Filesystem paths and IndexedDB are not accepted. See
[`docs/api/wasm/`](docs/api/wasm/) for the spec and
[`examples/wasm-playground/`](examples/wasm-playground/) for a Vite + React
consumer. Build from source with `make build-wasm`.

## Features

| Area | What it covers |
|---|---|
| **openCypher TCK** | 3892 of 3892 pinned [openCypher TCK](https://github.com/openCypher/openCypher/tree/master/tck) 2024.3 scenarios pass, with 0 skips — one CIP-271-superseded `toString()` outline is removed from the pin itself (ADR-0188), so the claim is against the pinned corpus, not unmodified upstream 2024.3. The run is strict (it checks error class, error phase, and side effects) and is enforced in CI. Results live in [`docs/tck/compliance.md`](docs/tck/compliance.md). |
| **CIP (Cypher 25)** | 5707/5745 scenarios across 130 triaged CIPs of the CIP-derived eval suite — a **stretch scoreboard and ratchet gate**, not a language floor. The TCK is the floor; this only ratchets up. |
| **Cypher language coverage** | Reading/writing (`MATCH`, `OPTIONAL MATCH`, `WHERE`, `CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE`, `FOREACH`, `INSERT`), composing (`WITH`, `UNWIND`, `UNION`, `CALL {}` subqueries, subquery expressions), schema DDL (indexes, constraints, graph types), 100+ built-in functions, and GQL/GPM pattern matching (label expressions, quantified paths, path selectors, `CYPHER 5`/`CYPHER 25` prefixing), and query introspection with `EXPLAIN`/`PROFILE` (per-operator plan, rows, and time — see the [query profiling](#query-profiling) row). |
| **Vector search** | `vector()` properties, HNSW when present (otherwise brute-force cosine similarity), `db.index.vector.queryNodes` top-k search, similarity/distance functions, and hybrid vector + graph queries. Three-tier search is withdrawn with Lance (ADR-0052). |
| **Geospatial** | WGS-84 and Cartesian points (with 3D), geometry types, R-tree `CREATE POINT INDEX`, distance and spatial predicates, bounding-box/radius queries, and WKT/GeoJSON conversion. |
| **Query profiling** | `EXPLAIN` renders the plan tree (operator, details) in table mode and a structured tree in the machine modes; `PROFILE` executes and reports per-operator rows and time (ADR-0344/0345) — profiling costs nothing on non-PROFILE runs. See [`docs/api/cli/spec.md`](docs/api/cli/spec.md#explain-and-profile-output-adr-03440345). |
| **CLI shell** | Interactive REPL with readline-style editing and in-session history (no tab completion or history file — ADR-0073); SQLite-style dot-commands; output modes `table`, `csv`, `json`, `jsonl`, `markdown`, `cypher`. |
| **Import** | `LOAD CSV` directly in Cypher queries (the `.import`/`.export` dot-commands are withdrawn). |
| **Clients** | Rust (`cypherlite::Database`), Python + Arrow IPC, JS/Java/Go via the `cypherlite-ffi` C ABI, and Bolt as remote transport. |

### Non-Interactive Mode

```bash
# Execute a single query
cypherlite ./mydata "MATCH (p:Person) RETURN p.name"

# Execute from a file
cypherlite ./mydata < setup.cypher

# Pipe-friendly JSON output
cypherlite -json ./mydata "MATCH (p:Person) RETURN p.name"

# JSON snapshot codec (empty directory only)
cypherlite --snapshot-format json ./mydata-json "RETURN 1"

# In-memory database
cypherlite :memory: "RETURN 'hello'"
```

### Command-Line Flags

```
-h, --help    Show usage and exit
--version     Show version and license and exit (with --json: one JSON object)
--json        Output as JSON
--jsonl       Output as JSON Lines
--csv         Output as CSV
--markdown    Output as Markdown
--noheader    Disable column headers
--mode MODE   Set output mode (table, csv, json, jsonl, envelope, markdown, cypher)
                default: table on a terminal, jsonl when piped; toon when agent mode is on (ADR-0426)
--param N=V   Bind a Cypher parameter (repeatable; V parsed as a JSON scalar, bare words are strings)
--fetch-params N=SRC  Fetch a URL or read a file (http, https, file path), parse it as JSON, and bind it as the parameter N (repeatable)
--snapshot-format F            Snapshot codec (parquet|json); empty directory only
--wal-dir DIR                  WAL directory (default <DATABASE>/wal)
--checkpoint-interval-secs N   Timed checkpoint period; 0 disables
--durability-mode MODE         WAL durability (full|everysecond|snapshot; default full)
--logical-graph-size-limit SIZE  Live graph + index cap in logical bytes (bytes or mb/mib/gb/gib)
--max-rows N                   Print at most N rows per statement; 0 = unlimited (default 100
                               under agent mode, else unlimited)
--agent-mode / --no-agent-mode  Force agent ergonomics on/off (ADR-0407's ladder: the flag, then
                               CYPHERLITE_AGENT_MODE=1/0, then harness detection — piped runs)
--read-only                    Open an existing database without writing or checkpointing (:memory: rejects writes)
--follow                       Tail the WAL of a native directory (implies --read-only)
--load-csv-dir PATH            Allow local LOAD CSV under this directory (default denied)
--namespace NAME               Session namespace (neo4j|system|system_ontologies; default neo4j)
-c, --command Q                One-shot query (same as positional QUERY)
```

Lifecycle commands are positional: `cypherlite skill install|remove [--agent NAME] [--force]` manages the agent-skill bundle, and `cypherlite upgrade [--yes] [--check]` (alias `update`) self-updates the binary.

Flags accept both single- and double-dash forms (`-json` or `--json`). `--` ends flag parsing so any following argument is treated as positional. An unknown flag or an invalid or missing `--mode` value is a usage error (exit code `2`); a rejected statement (syntax, semantic or type error) exits `6`, a write conflict or a database locked by another opener exits `5`, and any other runtime failure (database open error, query execution error, failed final checkpoint on a persistent database) exits `1`. `cypherlite <command> --help` prints one command's synopsis and flags (e.g. `cypherlite clone --help`); top-level `cypherlite --help` lists the query-run flags plus every command compiled into the binary. The full CLI reference — flags, positionals, env vars, exit codes — lives in [`docs/api/cli/spec.md`](docs/api/cli/spec.md).

`--fetch-params N=SRC` fetches an `http(s)` URL or reads a file, parses it as JSON,
and binds the document as the parameter `N` — the same conversion `--param`
uses, without quoting a large document onto the command line (ADR-0267). Each
source is capped at 64 MiB with a 60 s timeout; the fetch runs in the CLI
process, so the engine's `LOAD CSV` policy is untouched. One invocation covers
the fetch → bind → `UNWIND` → `MERGE` pipeline:

```bash
cypherlite ./so.cl \
  --fetch-params 'value=https://api.stackexchange.com/2.3/questions?tagged=neo4j&site=stackoverflow' \
  -c 'UNWIND $value.items AS q MERGE (:Question {id: q.question_id})'
```

#### For agents and scripts

Output and errors are designed to be parsed:

- **Result shape.** `--json` prints a bare array of row objects and `--jsonl` one object per row. `--mode envelope` prints one `{"columns":[…],"rows":[…],"truncated":false}` object per result set, so an empty result still lists its columns ([spec](docs/api/cli/spec.md#result-envelope)).
- **Errors.** In `json`, `jsonl` and `envelope` modes a failed statement also prints `{"error":{"code","exit_code","kind","message","retryable"}}` on stdout, after any rows already written (a syntax error adds `column` and `source`, and stderr shows the line with a caret under the token); the first line of the message on stderr is unchanged. Branch on `code`: `validation_error` (exit 6), `conflict` (exit 5, `retryable` only for a write conflict), `fatal_error` (exit 1). Usage errors exit 2 ([spec](docs/api/cli/spec.md#query-run-errors)). When a run of several statements stops on a failure the error also says how far it got (`statement`, `statements`, `committed`), so you know whether the store changed ([spec](docs/api/cli/spec.md#progress-of-a-multi-statement-run)).
- **Row cap.** Under agent mode — a detected agent harness (Claude Code, Codex, Cursor, Gemini CLI, Copilot, the generic `AI_AGENT`/`AGENT` markers, …) with stdout not a terminal, or an explicit `--agent-mode`/`CYPHERLITE_AGENT_MODE=1` ([ADR-0407](docs/api/cli/spec.md#agent-mode-adr-0407)'s ladder) — a statement prints at most 100 rows, warns on stderr, and still exits `0`. The cut rides **in-band** in every machine mode (ADR-0403) — `--jsonl` ends in a terminal `{"rowCap":{"kept":N,"total":M}}` line, `--mode envelope` carries `"truncated": true` plus `"rowCap"`, `--json` wraps as `{"data":[…],"rowCap":{…}}`, `toon` prints a `rowCap:` entry — so a capped result never reads as complete. Raise or lift it with `--max-rows N` / `--max-rows 0`, or set `CYPHERLITE_MAX_ROWS`. Terminals and scripts outside a harness are unchanged. The cap limits what is printed, not what the engine computes, so bound the work with a Cypher `LIMIT` ([spec](docs/api/cli/spec.md#row-cap)).
- **Version.** `cypherlite --version` prints the `CypherLite v<version>` line plus `License: Apache 2.0 (binary)`; `--version --json` prints `{"arch","license","name","os","version"}` with the `license` member `Apache 2.0 (binary)` (ADR-0432).
- **Help.** `--help` and `<command> --help` end with one or two examples; set `CYPHERLITE_HELP_EXAMPLES=0` to hide them.
- **Typos.** An unknown flag suggests the nearest real one (`--readonly` → `Did you mean '--read-only'?`), for the query run and for each command's own flags.

### Agent skill

`cypherlite skill install` installs the embedded cypherlite skill bundle — a `SKILL.md` plus CLI, Cypher, vector, and spatial references — into every detected AI agent's skills directory. Supported/detected agents: claude-code, cursor, windsurf, copilot, antigravity, gemini-cli, cline, codex, pi, opencode, junie.

The bundle is version-stamped, and `cypherlite upgrade` refreshes it automatically after an upgrade. Each detected agent's `<skills dir>/cypherlite` is classified **Missing** (absent), **Owned** (`.cypherlite-installed` marker present), **Legacy** (no marker, but `SKILL.md` frontmatter declares `name: cypherlite` and contains a `version:` line), or **Foreign** (exists but neither, including a `SKILL.md` declaring `name: cypherlite` with no `version:` line). Installed = Owned ∪ Legacy. An installed bundle is updated with no prompt (a plain `skill install` — the child adopts a recognized Legacy bundle itself, re-validating ownership at use); when nothing is installed anywhere, an interactive upgrade asks `Install the cypherlite skill? [Y/n]` (empty means yes), `--yes` installs without asking, and a non-interactive run without `--yes` prints `run: cypherlite skill install`. A **Foreign** directory is never overwritten: the upgrade prints `note: {path} is not a recognizable cypherlite skill (no .cypherlite-installed marker, and SKILL.md does not declare name: cypherlite with a version line); leaving it untouched` and points at `cypherlite skill install --force`; when a Foreign directory coexists with installed or missing agents it falls back to one `cypherlite skill install --agent <name>` per non-foreign agent. The upgrade exit code stays `0` in every case. `cypherlite skill remove` removes the bundle (idempotent). `--agent NAME` scopes either action to a single agent (case-insensitive). Unknown names, and zero detected agents, are errors that list the valid agent names.

`skill install` writes `<skills dir>/cypherlite/.cypherlite-installed`, the ownership marker proving the directory was created by cypherlite. Without `--force`, `skill install` replaces a destination that is missing, marker-owned, or **a recognizable cypherlite bundle** — no marker, but `SKILL.md` frontmatter declares `name: cypherlite` and contains a `version:` line. Any other existing destination (user-authored or third-party content, or a symlinked skills directory) is refused: it exits `1`, leaves the directory untouched, and prints a `no .cypherlite-installed marker present` error. `skill remove` is stricter — with no replacement bundle to fall back on it removes only a marker-owned directory and refuses any unmarked one the same way. `--force` explicitly clobbers any destination on either command.

```bash
# Detect agents and install the skill bundle into each
cypherlite skill install

# Install into one agent only
cypherlite skill install --agent claude-code

# Remove the bundle (idempotent)
cypherlite skill remove

# Override the ownership refusal for a hand-created skills directory
cypherlite skill remove --force
```

## Telemetry

The `cypherlite` CLI reports **anonymous** usage metrics — command shapes,
feature flags, an anonymous install id, and a coarse environment tag. It never
sends query text, file paths, or your data. Collection defaults to on, with a
layered opt-out:

```bash
cypherlite telemetry off       # persist the opt-out
cypherlite telemetry status    # e.g. "disabled (config file)"
```

`DO_NOT_TRACK=1` or `CYPHERLITE_TELEMETRY=0` also disable it. Only the
standalone CLI collects metrics; the embedded Rust and Python APIs do
not. See the [CLI reference](https://neo4j-labs.github.io/cypherlite/docs/reference/cli-flags)
for the full contract.

## Storage Backends

CypherLite persists through a virtual filesystem (ADR-0072): a WAL plus **one
snapshot codec** (ADR-0050), either as a directory (Native VFS) or
as a single host file (`.cl` bundle, ADR-0074). Do not mix Parquet shards and
`graph.json` in the same directory.

Durability defaults to `full` (acknowledge after the WAL barrier).
`everysecond` can lose the latest approximately one second of acknowledged
writes; `snapshot` can lose all writes since the latest completed checkpoint.
See `--durability-mode` and the [storage guide](docs/guides/storage-engine.md).

### In-Memory

No persistence — data lives only in RAM and is lost when the process exits. The fastest option, ideal for testing, demos, and throwaway exploration.

```bash
cypherlite                   # default (no path = in-memory)
cypherlite :memory:          # explicit
```

### Parquet snapshot (default)

WAL + Parquet shards (`manifest.json`, `nodes/{lo}-{hi}.parquet`, `relationships/…`). Checkpoints rewrite dirty id windows.

```bash
cypherlite ./mydata
cypherlite ./mydata "MATCH (n) RETURN n"
cypherlite ./mydata < setup.cypher
```

### JSON snapshot codec

Same WAL; checkpoints rewrite CypherLite GraphData to `graph.json`. `--snapshot-format` / `CYPHERLITE_SNAPSHOT_FORMAT` apply only to an empty directory. Import a legacy `.db` file by copying it to `<dir>/graph.json`.

```bash
cypherlite --snapshot-format json ./mydata-json
CYPHERLITE_SNAPSHOT_FORMAT=json cypherlite ./mydata-json
```

### Single-file bundle (`.cl`)

A path ending in `.cl` selects the bundle VFS (ADR-0074): WAL, snapshot
objects, and catalog live in **one host file** — no sidecar `-wal` files or
`wal/` directory. A missing `foo.cl` creates a new container. Both snapshot
codecs (Parquet default, or JSON via `--snapshot-format json`) work inside a
bundle; `--wal-dir` is rejected on `.cl` paths.

```bash
cypherlite ./mydata.cl
cypherlite ./mydata.cl "MATCH (n) RETURN n"
```

### Choosing a codec

| | In-Memory | Parquet | JSON codec |
|---|---|---|---|
| Persistence | None | Directory or `.cl` bundle | Directory or `.cl` bundle |
| Snapshot | — | Parquet shards | `graph.json` |
| Mix with the other product codec | — | No | No |
| Best for | Testing, demos | Default durable store, Python | Readable GraphData / import |

See the [Storage Engine guide](docs/guides/storage-engine.md) for WAL and codecs.

## Development

```bash
# Build all crates
cargo build

# Build optimized release
cargo build --release

# Run all tests (or: make test)
cargo test -p cypherlite -p cypherlite-front -p cypherlite-executor -p cypherlite-cli \
           -p cypherlite-splitter -p cypherlite-tck

# openCypher TCK conformance suite (strict; 3892/3892 pass, 0 skips)
make test-tck

# CIP evaluation suite — Neo4j Cypher beyond the TCK (scoreboard + ratchet gate)
make cip-eval

# Lint (CI enforces both)
make lint

# Python package development (or: make install-python && make test-python)
pip install neo4j-cypherlite   # pending PyPI publish; else:
pip install maturin
cd surfaces/cypherlite-python && maturin develop --release
python -c "import cypherlite; print(cypherlite.connect(':memory:'))"

# Python package Rust-side tests
PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 cargo test -p cypherlite-python

# Documentation — deep guides and references are plain markdown in docs/
ls docs/guides docs/reference
```

Architecture, storage, protocol, dialect, public-API, and dependency
decisions are recorded as Architecture Decision Records in
[`docs/adr/`](docs/adr/). Once the ADR is drafted and its file is about
to be created — never earlier, a mint is irreversible — claim the next
number with `python3 scripts/adr_mint.py` (ADR-0300). The mint
prints the claim commit SHA, which the new ADR must carry as a
`Claim:` header line; the `adr-numbers` CI check enforces it
(ADR-0301). See [AGENTS.md](AGENTS.md) for the full rules.
