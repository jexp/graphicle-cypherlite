# Git-to-graph plan — commit history as a first-class citizen of the store

_2026-10-30. Companions: `research/git-history-and-codescene.md` (gap analysis, metric priorities), `architecture-concepts.md` (§3 churn-elevation cartography, §7 concept sources), `supply-chain-plan.md`, `language-coverage-plan.md`. Sources researched: CypherLite's `lab git-to-graph`/`fs-to-graph` (Apache-2.0, verified in `/Users/flm2dxqwm5/d/llm/cypherlite` at 0.13.0) and the kontext-e jQAssistant Git plugin (GPL-3.0 — **ideas only, no code**)._

## 0. The decision this plan implements

Graphicle's model is pre-built for history and never wired: the migration ported `gf_commits`/`gf_commit_parents` to `(:Commit {oid, tree_oid})-[:PARENT {position}]->(:Commit)` (`migrate/04_commits.cypher`) and `deletion.rs` maps the label — but nothing writes it. Everything in the CodeScene gap analysis (hotspots, temporal coupling, knowledge/bus-factor) and the cartography elevation maps waits on one thing: **the commit writer**. This plan is that writer, plus the link from history to the graph we already build.

## 1. What we steal, from whom

From **CypherLite** (Apache-2.0, liftable code — ~1,400 lines, but we re-embed the ideas in `graphicle`'s store, not run their CLI):
- The **`git log` subprocess discipline**: null stdin, no shell, `--no-pager --no-merges --no-color --no-ext-diff --no-textconv -z`, record/field sentinels `0x1e`/`0x1f`, mailmap-resolved `%aN/%aE`, strict parse-as-pure-function with per-record errors, `parse_iso` for dates. No git2/gix dependency.
- **Bounded ingestion**: 128 MiB log cap, per-1,000-row `UNWIND` batches, one explicit transaction per batch, MERGE idempotence on natural keys, resource-budget checks *before* any write.
- The **`CO_CHANGED` pair-capping lesson**: skip commits whose changeset exceeds a bound (default 50) — large merges would explode pair counts; cap total pairs; and the caveat that a *narrower* re-import leaves stale pairs → our recompute must be window-keyed (§5).
- The **query catalog** in their skill reference (churn hotspots, sole-owner files, co-change coupling, untracked-vs-git-only) — copy the questions, rewrite the Cypher against our schema.

From **kontext-e jQAssistant** (GPL-3.0 — model and patterns only):
- **The Change node as the join point**: `(:Commit)-[:CONTAINS_CHANGE]->(:Change {kind})-[:MODIFIES]->(:File)` rather than a bare commit→file edge. `kind` `A/M/D/R/C` per (commit, path). This is what makes "who touched this symbol" and co-change-at-symbol-level natural.
- **Authors and committers as separate persons** (`AUTHORED`/`COMMITTED`), person dedup on lowercased email, name from newest commit, plus an `aliases` list for the same human under several idents over the years — the published reference is the jqa-dashboard's author-dedup rule pair (softvis-research/jqa-dashboard, `dashboard.adoc`; see `research/jqa-git-queries.md`).
- **Ordered parents** (`HAS_PARENT` with position; >1 = merge), branches and tags as own nodes, HEAD pointer.
- **The anchored-lookup rule** (their hardest-won lesson): no git property is globally unique — the same sha appears in forks, `pom.xml` in every repo — so every lookup anchors through the repository/project node. In our store: `MATCH (p:Project {id:$repo})-[:HAS_COMMIT]->(c:Commit {oid:$sha})`. **With the supernode caveat**: the anchor *becomes* a supernode by design once every commit, change, person and branch hangs off it. Queries go **against** the anchor — start at it by key and expand outward (`(p:Project {id:$r})-[:HAS_COMMIT]->(c:Commit {sha:$s})`), or skip the edge entirely and scope by the `repository_id` property + range index the way the rest of the store does — but never **across** it (no pattern that arrives at `:Project` from one side and leaves through the other; that is a dense-node expansion on every match).
- **Resume-from-head incremental**: query the graph for the branch's last scanned head sha, then `sha..branch`.
- **Rename/copy modeling**: rename = `DELETES old + CREATES new` + an old→new name chain to follow, so path-history queries survive moves (we already have `(:SymbolMove)` for symbols — files get the same treatment). With one deviation, per `research/jqa-git-queries.md`'s recommendation: **handle renames at load time** — write the `:Change` against the *new* path with an `old_path` property, rather than graph-walking `HAS_NEW_NAME` chains in Cypher (kontext-e's chains exist because their scanner can't rewrite history at scan time; our writer can, and every churn query then needs no rename special case).

What we deliberately do **better than both** (they can't): **materialize the join from history to code at write time** — a `Change` resolves against our bi-temporal tree to `:File` *and* `:Symbol` nodes (via `relpath` + line ranges), and commits correlate with `:Episode`s. jQA leaves this to path-string matching at query time, which renames break; CypherLite has no code side at all.

## 2. Graph model (target schema)

```
(:Project)-[:HAS_COMMIT]->(:Commit {oid, tree_oid, subject, message, author_date, commit_date, known_at})
(:Commit)-[:PARENT {position}]->(:Commit)                      -- ordered; >1 parent = :Merge
(:Project)-[:HAS_BRANCH]->(:Branch {name, ref})-[:AT]->(:Commit)
(:Project)-[:HAS_TAG]->(:Tag {name})-[:TAGS]->(:Commit)
(:Person {email, name})-[:AUTHORED {at}]->(:Commit)
(:Person)-[:COMMITTED {at}]->(:Commit)
(:Commit)-[:CONTAINS_CHANGE]->(:Change {kind: 'A'|'M'|'D'|'R'|'C', old_path?, new_path?})
(:Change)-[:MODIFIES]->(:File)                                  -- our content-store :File {path}
(:Change)-[:TOUCHES {lines?}]->(:Symbol)                        -- symbols intersecting the change (best effort)
(:File)-[:CO_CHANGED {window, count}]->(:File)                  -- derived, window-keyed
(:Symbol)-[:CO_CHANGED {window, count}]->(:Symbol)              -- derived, the agent-actionable one
(:Episode)-[:AT_COMMIT]->(:Commit)                              -- head set == commit tree
```

Principles carried over from the migration's own rules:
- Commits are **immutable facts**: `author_date`/`commit_date` are the *valid* time, `known_at` the *record* time (first ingest). They take part in the bi-temporal model the way `(:Assertion)-[:ON_TIMELINE]` does, but never close (no `*_HIST` flips) — history doesn't change, only our knowledge of it does (a rebase/squash forces a §6 rescan, like kontext-e's documented limitation).
- Everything anchors through `(:Project)` (the anchored-lookup rule).
- `CO_CHANGED` is **derived data with an explicit window key** (`window: 'all-time' | '90d' | …`) — recomputed by deleting the window's pairs and rewriting, never `SET count` in place. That fixes CypherLite's stale-pairs caveat at the cost of a derived-data job, which is what our job queue is for.
- The `:Change`→`:Symbol` resolution is *episode-lagged by design*: it resolves against the episode whose head set the commit's tree matches when one exists, else against the file's then-current head — good enough for knowledge metrics, exact for tip commits.
- **The anchor is a supernode; query against it, never across it.** Bulk paths (ingest batches, recompute) scope by `repository_id` property + range index; point lookups start at `(:Project {id})` by key and expand outward through one typed edge. No pattern traverses through the project node to the far side, and no unbounded expansion leaves it without a label/rel-type constraint. Every `:Commit`/`:Change` also carries `repository_id`, so the anchor edge is a convenience for point reads, not a join path.

## 5. Store topology: history in its own database, merged on demand

CypherLite makes opening a database nearly free (per-directory stores, `:memory:` scratch, `clone`) — exploit that instead of one ever-growing store:

- **One database per repo, or per repo-set without forks.** Git history is immutable, append-mostly, large and query-local — the best-behaved tenant for its own store. A repo-set database (upstream + true forks collapsed, duplicates deduped by sha) keeps the **per-repo identity hashes honest**: a store's state digest covers exactly one set-of-repos, never double-counted fork history.
- **Bring data together on demand**: an analysis or visualization that needs history + code graph + supply chain opens the pieces it needs and merges them into an **ephemeral analysis database** (`:memory:` or a scratch directory): `graphicle` already uses `:memory:` staging this way for graph_materialize and expansion. The merged copy is throwaway; the tenants stay small, independently checkpointed and independently deletable.
- **Never merge by writing into a live tenant.** Merge direction is always tenant → ephemeral, so a long-running analysis never blocks a sync.
- The Neo4j equivalent of this topology is **multidb** — see `neo4j-backend-plan.md`: the same per-repo(-set) database maps to a Neo4j database, selected per connection.

This topology also bounds the §6 rescan cost (history re-ingest touches one tenant) and keeps the deletion/receipt machinery's per-store semantics intact.

## 3. Ingestion mechanics

One `git log` call per sync, extending CypherLite's argv with what they omit (parents, committer, status, renames, bodies):

```
git -C DIR -c log.showSignature=false --no-pager log --no-color --no-ext-diff \
    --no-textconv --name-status -z -M --format=%x1e%H%x1f%T%x1f%P%x1f%aN%x1f%aE%x1f%aI%x1f%cN%x1f%cE%x1f%cI%x1f%s%x1f%b%x1f -- .
```

`--name-status -z -M` gives `A`/`M`/`D`/`Rold⇒new` per path with rename detection on. Merge commits are *included* (we want the DAG; CypherLite excluded them because they had no parent edges — we do). Branches/tags/HEAD via `git for-each-ref` + `git rev-parse HEAD` (two cheap extra calls, kontext-e parity). `--since`/`--max-count` expose as config for first-sync bounding. All the parse discipline, caps and batch/transaction shape from CypherLite's implementation apply verbatim in spirit.

**Episode correlation**: after each sync's projection, the episode digest is a function of the head set (and, since the 0.1.9 port, the engine's graph format); when the working tree is clean at HEAD, the head set *is* the commit tree, so the sync writes `(:Episode)-[:AT_COMMIT]->(:Commit {tree_oid})`. On a dirty tree, no edge — the episode stays a working-tree snapshot. `graphicle pr`'s scratch projects, which record commit trees, get the edge for free.

## 4. Where it runs

- A **job kind** (`history`), enqueued by sync after projection, keyed `(repository, kind, input_hash)` where `input_hash` = (HEAD sha, history window config, format version). Resumable, cacheable, single-writer — the invalidation machinery already tells us when sources moved; a `:Change`-touching push re-enqueues.
- First run on a project **back-fills full history** bounded by config (default: all, capped by the log-byte budget); later runs resume from the recorded branch head (the kontext-e pattern), so the steady-state cost is one short `git log old..new`.
- `CO_CHANGED` derivation is its own job kind (`coupling`) parameterized by window; symbol-level coupling resolves `:Change`→`:Symbol` first. Deliberately *not* in the sync hot path.

## 6. Rescan semantics

Branch deletion, rebase and squash are not incrementally detectable (kontext-e's documented limit, and ours): if a resumable walk finds the recorded head unreachable (`git merge-base --is-ancestor` check fails), the job marks the project's history `:Stale` and the next run does a full re-ingest under a new `known_at` — old `:Change`/`:CO_CHANGED` rows for vanished commits are deleted (they are derived/anchored facts, not receipt-protected content; the deletion machinery's label walk already knows `Commit`).

## 7. Prerequisite: vendor bump — DONE (2026-10-30, `6c2d6fca`)

`third_party/cypherlite` was a shallow, tag-pinned checkout at 0.12.4 (`remote.origin.fetch` followed only `refs/tags/v0.12.2`); it is now unshallowed on **main @ `221329d5` = v0.13.0 + 51 commits** with the full refspec. Graphicle compiles clean against it (three mechanical `store_graph.rs` fixes: `Arc<PropertyMap>` properties, `QueryResult.header.columns`), and existing `reviews.graph` stores are byte-compatible (the on-disk changes since 0.12.4 apply only to stores that set a `db.ontology` document, which graphicle never does). The lab `git-to-graph` reference code (0.12.7+) and the mature ingestion shape (0.13.0: per-batch transactions, the 5M pair cap) are now readable in the vendored clone at `surfaces/cypherlite-cli/src/lab/`. The parity suite must be re-run against the new engine — episode ids digest the engine's `graph_format`, so the first sync re-projects by design.

## 8. Feed for gbuild (node sketch)

- **`git-log-reader`** (M): the `git_log.rs`-style pure parser (parents, committer, name-status -M, bodies) + fixture builder (cypherlite's `cypherlite-test-support/src/git.rs` is the model; ours lives in `graphicle`'s it-fixtures). Acceptance: parse corpus of real logs incl. renames/merges/octopus; malformed record → typed error naming it.
- **`history-writer`** (M): the §2 schema as UNWIND batches, anchored MERGEs, per-batch transactions, caps; `history` job kind + resume-from-head. Acceptance: ingest this repo's full history; kill -9 mid-run, resume, idempotent counts; forked repo re-ingest does not cross-link (anchored lookups).
- **`episode-correlation`** (S): `AT_COMMIT` on sync/pr; dirty-tree no-edge. Acceptance: clean-tree sync ⇒ edge; `graphicle pr` scratch projects get edges.
- **`change-symbol-resolution`** (M): `:Change`→`:File`/`:Symbol` against the matching episode. Acceptance: rename chain + symbol-move chain both resolve to the same surviving `:Symbol`.
- **`coupling-job`** (M): window-keyed `CO_CHANGED` at file and symbol level, changeset cap, recompute-not-update. Acceptance: narrower window never leaves stale pairs; a known-coupled pair in this repo surfaces.
- **`history-queries`** (S): the metric pack as plain Cypher behind `graph.*` resources / MCP — hotspots (degree × churn), sole-owner files, knowledge loss (author gone + churn high), co-change joined against `CALLS`/`IMPORTS` (temporal coupling), each cited with `path:line` evidence. **Start from `research/jqa-git-queries.md`**: 13 collected queries (9 published — the jqa-dashboard author-dedup pair, the JohT change-coupling pipeline with support/lift/jaccard filters, the SO files-by-author; 4 ours: bus factor, datetime churn, ICSME'17-style knowledge loss, rename-aware churn), each already ported to our schema. Acceptance: answers on this repo match `git log | wc` sanity numbers.
- **`vendor-bump-cypherlite`** (DONE, `6c2d6fca`): 0.12.4 → main 221329d5, format compatibility verified for ontology-free stores.

Sequencing: reader → writer → correlation → resolution → coupling → queries (the vendor bump prerequisite is done). The first four already unlock the CodeScene research's recommendations (1)–(3) and the architecture-concepts cartography elevation. The §5 topology is orthogonal: the writer can land against the single-store world first, then move history into its own database once the merge-on-demand helpers exist.
