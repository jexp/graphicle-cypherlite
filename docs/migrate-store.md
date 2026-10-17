# Migrating a SQLite store to the CypherLite graph store

`graphicle migrate-store` is the one-time, forward-only importer for
installations that have a legacy `reviews.sqlite3` store (migration plan
§5 of `cypherlite-migration.md`). It works in two stages with no SQLite
dependency in the runtime:

1. **Export.** The standalone script `scripts/migrate/export_store.py`
   (Python 3 standard library only; the `graphicle` binary embeds and runs
   it, so no file lookup is needed) dumps each migrated table to one CSV,
   deterministically (fixed columns, `ORDER BY` on the primary key), and
   prints a JSON manifest of row counts. The source store is opened
   read-only and is never modified — it stays behind as the backup.
2. **Import.** The per-domain Cypher scripts in
   `crates/graphicle/src/migrate/*.cypher` load the CSVs with
   `LOAD CSV WITH HEADERS` and upsert with `MERGE`, `MATCH`ing the nodes a
   relationship hangs off (credentials to their workspace, projects to
   their workspace, objects to their project, commits to their parents,
   assertions to their project). Each section carries a count query; the
   command compares it against the export manifest and fails the migration
   on any mismatch.

## Usage

```sh
graphicle migrate-store ~/.graphicle/reviews.sqlite3 ~/.graphicle/reviews.graph [--json]
```

- The **target directory must be empty or not exist** — a non-empty target
  is refused, never overwritten. Run the migration once, onto a fresh
  directory.
- The **source file is left untouched** (read-only open); keep it as the
  backup until the new store has proven itself.
- Requires `python3` on `PATH` (standard library only).
- The report lists, per domain, the exported row count and the node count
  found in the graph after import; any mismatch fails the command.

Standalone use of the pieces (what the command does for you):

```sh
python3 scripts/migrate/export_store.py reviews.sqlite3 /tmp/export
# then run the .cypher scripts against a store opened with the LOAD CSV
# directory scoped to /tmp/export (cypherlite OpenOptions.load_csv_dir or
# CYPHERLITE_LOAD_CSV_DIR), passing each section's $csv as a file:/// URL
# plus the $EE/$EN/$ER/$PH/$E/$NL/$CR decoding parameters.
```

## What is imported

| Domain | Source table(s) | Graph shape |
|---|---|---|
| Workspaces | `workspaces` | `(:Workspace)` |
| Credentials | `credentials` | `(:Credential)-[:IN_WORKSPACE]->(:Workspace)` |
| Projects | `repo_projects` | `(:Project)-[:IN_WORKSPACE]->(:Workspace)` |
| Objects | `repo_objects` | `(:Project)-[:HAS_OBJECT]->(:Object)` |
| Memories | `repo_memories` | `(:Memory)` (scoped by `repository_id`) |
| Symbols | `gf_nodes` (live rows only) | `(:Symbol)-[:HAS_VERSION]->(:SymbolVersion:Live)` |
| Commits | `gf_commits`, `gf_commit_parents` | `(:Commit)-[:PARENT {position}]->(:Commit)` (`object` as base64) |
| Timeline | `gf_timeline` | `(:Assertion)-[:ON_TIMELINE]->(:Project)` (rowid carried as `seq`) |
| OAuth accounts | `am_oauth_accounts` | `(:OAuthAccount)` (secret-id references only) |
| Provider identities | `am_provider_identities` | `(:ProviderIdentity)` |
| Provider emails | `am_provider_emails` | `(:ProviderEmail)` |

## What is not imported

- **Projected history.** Expired `gf_nodes` versions, closed edges,
  episodes, revisions (`repo_memory_revisions`), event logs, and deletion
  receipts are not carried over; re-running a sync/projection rebuilds
  derived history. Only the live state moves. Commit history is the
  exception: a commit is a fact about the repository, not a projection, so
  `gf_commits`/`gf_commit_parents` are migrated.
- **Secrets.** `am_secrets` is intentionally left behind: the graph store
  keeps secrets as workspace-key-encrypted `(:Secret)` nodes, and a
  plaintext SQLite vault cannot be re-keyed into the new store.
  `(:OAuthAccount)` nodes keep their `token_secret`/`refresh_secret`
  references (and the columns' data), but the referenced secrets do not
  exist, so connected accounts must be reconnected after migrating.
- **Everything not listed above** (runs, jobs, connectors, claims,
  entities, vectors, coverage, …) stays behind in the SQLite file. If you
  need more domains, extend `DOMAINS` in `export_store.py` and add a
  section to the matching `.cypher` script — the file format is
  `-- @csv <file>` / `-- @verify <count query>` markers over one statement.

## Caveats

- **Forward-only.** There is no path back from the graph store to SQLite;
  the untouched `reviews.sqlite3` is the rollback.
- The CSV interchange escapes newlines in text fields (the engine's
  `LOAD CSV` reads line-wise); text containing the control character
  U+0002 is rejected by the exporter rather than migrated corrupt.
- Local `LOAD CSV` is an opt-in engine capability; the command scopes it
  to the temporary export directory for the duration of the import.
