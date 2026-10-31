# Git history and CodeScene-style behavioral analysis in Graphicle

_2026-10-29. Evidence gathered from `main` (eccc97cc) and `jexp/feat/cypherlite-backend`. All file:line references are to the `main` checkout unless prefixed with the branch name._

## (a) What Graphicle has today

### Git integration that exists — all of it working-tree-scoped

Graphicle touches Git in five places, and **none of them capture commit history into the store**:

1. **Freshness hooks** — `crates/graphicle/src/git_hooks.rs:1-19,67-78`: `graphicle setup` writes a marked block into `post-commit`, `post-merge`, `post-checkout` that runs `graphicle sync <folder> --quiet` in the background. Git is used via `git rev-parse --show-toplevel` / `--git-common-dir` / `--git-path hooks` (git_hooks.rs:41-55) to *locate* the repo. No commit metadata is read or stored.

2. **Last-change dating for citations** — `crates/graphicle/src/source_age.rs:1-35`: one streamed `git log --relative --no-renames --format=…%ct --name-only -- <paths>` walk per context packet (`commit_dates`, source_age.rs:93-153), bounded by `GIT_BUDGET = 1500 ms` (source_age.rs:35). The result (last commit touching each file, else mtime) is used only to print "changed 2026-10-15 …" and "may be stale" marks in recall citations (`annotate`). Nothing is persisted; no author, no commit identity, no per-line data.

3. **Incremental rebuild hooks in the engine** — `crates/graphicle-graph/src/hooks.rs:1-40` and `hooks_runtime.rs:215-235`: the ported Graphify `post-commit` hook lists files the commit changed (`git diff --name-only -z`) and rebuilds only those files into `graph.json`. Again: git tells it *which files* changed; the commit itself is discarded.

4. **Gitignore-aware scanning** — `crates/graphicle-graph/src/detect_ignore.rs:1-10` ("anchored ignore rules, tracked-file exemptions"): git's ignore rules decide what is scanned, nothing more.

5. **PR tools (engine, opt-in)** — `crates/graphicle-graph/src/serve_prs.rs:14-40`: `get_pr_impact` shells out to `gh pr view` / `gh pr diff`, shows title, **PR author login**, CI, review, changed files, and the graph impact ("N nodes across M communities"). This is the only place any author name appears anywhere in the codebase, and it is display-only, not stored.

### The ghost of a real git-history feature

- **Schema exists, is never written**: `crates/graphicle/src/temporal_schema.sql:22-30` defines `gf_commits(oid, tree_oid, episode, committed_us, recorded_us, object BLOB)` and `gf_commit_parents`. The **only** code that touches `gf_commits` is a guard in `label_episode` — `crates/graphicle/src/graph_projection.rs:1070-1079`: a commit-sha label cannot be re-pointed at a different episode ("A verified Git commit cannot be relabelled"). There is no `INSERT INTO gf_commits` anywhere in the tree.
- **The writers were removed from this release**: `crates/graphicle/TEMPORAL_HISTORY.md:10-12,33-37` — "Commit SHAs that earlier releases registered with `graph-commit` remain labels of their episodes … registering Git commits (`graph-commit`, `graph.commit`) and walking their ancestry (`graph.ancestry`) are **not part of this release**." The integration test `crates/graphicle/tests/it/temporal_graph.rs:1-5` seeds legacy commit rows "directly, as those writers stored them" (unit `git-ancestry-writes`), because no writer exists to produce them.
- **The CypherLite branch carries the ghost forward but adds nothing**: `jexp/feat/cypherlite-backend:crates/graphicle/src/migrate.rs:374-382` and `crates/graphicle/src/migrate/04_commits.cypher:10-20` migrate legacy `gf_commits` rows into `(:Commit {oid, tree_oid, committed_us, recorded_us, object})-[:PARENT {position}]->(:Commit)`; `deletion.rs:161` maps the `Commit` label for deletion. `crates/graphicle/src/source_age.rs` on the branch is unchanged. The migration plan (`cypherlite-migration.md:94,195`) explicitly models commits and includes them in the `migrate-store` tool, but only as a port of the empty tables.

**Verdict for part 1: no, Graphicle does not integrate git history.** No git2/gix dependency exists in any Cargo.toml; every git interaction is a bounded `std::process::Command` invocation about the *working tree*. Commit/author/blame data is nowhere in the store. The schema, the episode-label convention, and the CypherLite `(:Commit)` model are all pre-built for it, though.

### What the graph *does* record that is behavioral-adjacent

- **Bi-temporal episodes** — `crates/graphicle/src/schema.sql:124-131`: `gf_episodes(repository_id, episode, sequence, created_at, node_count, edge_count, …, parent_episode)` — one per sync, kept forever (retention trims only coverage/cross-edges/unresolved-calls windows, defaults 20/20/2 episodes — `crates/graphicle/src/retention.rs:9-26`; "nodes, edges and episodes themselves are never removed").
- **Episode-versioned nodes/edges** — `schema.sql:135-160`: `gf_nodes`/`gf_edges` keyed with `valid_from_episode`/`valid_to_episode` (live = `valid_to_episode IS NULL`). History reads: `graph.as_of` with `as_of` (episode, label, sha-label, `latest`, `working`) or `valid_at`/`known_at`/`branch` (`graph_projection.rs:1364+`, `temporal_graph.rs:125+`); `graph.episodes` lists them (`graph_projection.rs:1321-1341`).
- **Symbol identity with content revisions** — `schema.sql:361-381`: `gf_symbol_heads(stable_id, revision, relpath, qualified_name, kind, ordinal, node_id, line_start, line_next, sha256)` where `revision` is "the hash of the symbol's own source lines" (`crates/graphicle-memory/src/graph/identity.rs:211-214`), plus `gf_symbol_moves` for exact rename/move detection across updates. So per-symbol *content change* between syncs is knowable today.
- **Communities with continuity** — `schema.sql:271-279`: `gf_communities(repository_id, episode, community_id, members, previous_id, overlap)` — Leiden/Louvain (`crates/graphicle-graph/src/cluster.rs:157,532-538`), with cross-episode community identity (`gf_community_ids`, stable ids, overlap).
- **Structural health proxies** — engine MCP resources `graphicle-graph://god-nodes` ("Top 10 most-connected nodes", `analyze.rs:253 god_nodes`), `://surprises` ("Cross-community surprising connections", `analyze.rs:522`), `://audit` (edge confidence), `://stats` (`crates/graphicle-graph/src/serve.rs:243-250`); plus file-level import cycles (analyze.rs module doc) and reverse-dependency impact traversal (`affected.rs:1-27`, 16 relation types).
- **Change impact between episodes** — `graph.impact` ("base-versus-head impact between two graph episodes", `crates/graphicle/src/api.rs:737-754`) — but it is edition-gated: core builds report `change_impact: false` and answer 501 (`core_capabilities.rs:31-38`).
- **Coverage per episode** — `schema.sql:399-426`: `gf_test_coverage` per (repository, episode, relpath), joined against changed symbols' source ranges.
- **The user's own plans already gesture at this**: `architecture-concepts.md:102` ("Cities/map renderings where *elevation = churn or violation density* …") and `:103` ("Charts (coupling over time, violations per episode) … the bi-temporal store makes 'architecture drift over time' a first-class query").

## (b) Gap analysis vs Tornhill / CodeScene

Metric list refreshed from [CodeScene's terminology](https://codescene.io/docs/terminology/codescene-terminology.html) and [code-health KPIs](https://codescene.com/blog/3-code-health-kpis/).

| CodeScene concept | Graphicle today | Gap |
|---|---|---|
| **Hotspots** (complexity × churn, at file/architecture/function level; X-Ray = function level) | Degree-based `god_nodes`; per-episode node/edge churn is *derivable* but not computed; **no complexity metric exists anywhere** (grep for complexity/cyclomatic/nesting-as-metric: nothing) | Full gap. Churn needs commits (or episode proxy); complexity needs extractor work |
| **Churn / development activity** | Episode windows in `gf_nodes`/`gf_edges` + symbol `revision` hashes give a *sync-granularity* change signal; `source_age` knows last-commit dates per file (ephemeral) | No commit-granularity churn; not aggregated or exposed |
| **Change coupling / temporal coupling** (co-change via same commit / same author+window / ticket) | Nothing. Communities are *structural*, not co-change based | Full gap; needs commit→file/symbol edges |
| **Code Health** (25-30 factors, 10→1 score) | Structural proxies only: god nodes, surprising cross-community edges, import cycles, edge-confidence audit | No complexity/length/duplication factors recorded on symbols |
| **Knowledge: Main Author, Bus Factor, Knowledge Islands, Knowledge Loss, Former Contributors** | No author data at all in the store (only PR author *displayed* in `serve_prs.rs`) | Full gap; needs `git log`/blame ingestion + a team roster (`repo_project_roles` exists as roster seed, schema.sql:330) |
| **Coordination needs / fragmentation** | Nothing | Full gap; same prerequisite as knowledge metrics |
| **Trend analysis / behavioral over time** | Episode DAG with per-episode counts, community overlap `CONTINUES`, symbol moves, coverage per episode, `graph.as_of` time travel | Partially present — but only *since first sync*; no pre-sync history, no commit timeline |
| **Delta analysis / PR quality gates** | `get_pr_impact` (graph impact of a PR, communities touched) and edition-gated `graph.impact` between episodes | Closest existing fit; no scoring, not stored |

What Graphicle has that CodeScene does **not**: the full typed dependency graph (CALLS/IMPORTS/INHERITS at symbol level), bi-temporal as-of queries, cross-repo edges, coverage joins, and serving all of it to coding agents over MCP at edit time.

## (c) Prioritized recommendations

Each maps onto the existing schema; Cypher forms cite the model in `cypherlite-migration.md:76-101` (`(:Symbol)-[:CALLS …]`, `(:Symbol)-[:HAS_VERSION]->(:SymbolVersion:Live)`, `*_HIST` rel types, `(:Episode)-[:FOLLOWS]->(:Episode)`).

### 1. Revive the commit writer — ingest git history at sync time — **S/M, highest value**

The model is already designed, schema'd, migrated, and deletion-mapped; only the writer is missing. At sync (and in the post-commit hook, which already fires), run `git log --format=… --name-status` bounded like `source_age`'s budget and write:

- main: `gf_commits` + `gf_commit_parents` (temporal_schema.sql:22-30) plus a new `gf_commit_files(repository_id, commit_oid, relpath, status)` (or reuse `repo_events`, schema.sql:67-70, for the raw log).
- branch: `(:Commit {oid, committed_us, author, author_email, message})-[:PARENT]->(:Commit)` and `(:Commit)-[:CHANGED {status}]->(:File)` / `…->(:Symbol)` by resolving paths against `gf_symbol_heads` of the matching episode. The sha-as-episode-label convention already exists (`label_episode`'s guard, graph_projection.rs:1070-1079) — wire it so an episode recorded at a clean HEAD gets its commit label automatically, making `graph.as_of as_of=<sha>` a real time machine.

Value: unlocks items 2, 4, 5, and trend back-fill before first sync — the single prerequisite for most of Tornhill's catalog. Effort S for oid/parents/author/message, M including per-file name-status and symbol resolution.

### 2. Hotspots — degree × churn — **S (after 1), high value**

A pure query over what then exists:

```cypher
MATCH (f:File)<-[:CHANGED]-(c:Commit) WITH f, count(c) AS churn
MATCH (f)<-[:CONTAINS*0..1]-(s)-[r]-() WITH f, churn, count(r) AS degree
RETURN f ORDER BY churn*degree DESC LIMIT 20
```

Complexity proxy today = degree (`god_nodes`, analyze.rs:253) and symbol line span (`gf_symbol_heads.line_start/line_next` — function length is effectively already stored). Even before item 1, an episode-churn variant works today: count episodes in which a node/edge window closed (SQL over `gf_nodes WHERE valid_to_episode IS NOT NULL` grouped by owner relpath). Effort S. Value: the single most-used CodeScene view, and it feeds agent context ("this file is a hotspot, be careful") which is exactly Graphicle's serving model.

### 3. Temporal coupling / change coupling — **M, high value, most natural fit**

From `(:Commit)-[:CHANGED]->(:File|Symbol` edges: co-change pairs within a commit (or author+time window) become weighted relationships:

```cypher
MATCH (c:Commit)-[:CHANGED]->(a), (c)-[:CHANGED]->(b) WHERE a <> b
WITH a, b, count(c) AS together
MATCH (a)-[r?]-() … // join against structural relations
```

Store as `(:File)-[:CO_CHANGED {weight, commits}]->(:File)` per episode window (fits the `*_HIST` versioning), or compute on demand as a `graph.coupling` operation. The killer feature vs CodeScene: join coupling against *structural* edges — `(a)-[:CO_CHANGED]->(b)` where `a` and `b` are in different communities, or coupled but not CALLS/IMPORTS-connected, is a refactoring to-do list an agent can act on. Effort M (pairwise explosion needs bounding; CodeScene uses a time window — do the same).

### 4. Code health — complexity factors from the ASTs Graphicle already parses — **M, medium-high value**

The tree-sitter grammars and extractors already walk every function; record cheap factors per symbol during extraction: body length (span already computed in identity.rs), nesting depth, conditional/branch count, parameter count. Add `SET s.complexity = …` on `(:Symbol)` per episode (bi-temporal for free), then a health score per file/community. This plus item 2 gives real hotspots (complexity × churn) and gives the `://god-nodes`/`://surprises` resources a quality dimension. Effort M across extractors, but per-language and incremental.

### 5. Knowledge metrics — authors, bus factor, knowledge islands — **M/L, medium value, more natural here than it looks**

From item 1's commits: aggregate per file/symbol authorship (`git log --numstat` or one bounded `git blame --line-porcelain` per hotspot file only), model `(:Author {email})-[:WROTE {lines, last_commit}]->(:File|Symbol)`. Bus factor / knowledge island are then one aggregation each. Knowledge *loss* needs former-contributor detection — a team roster; `repo_project_roles` (schema.sql:330) and the connectors/account data are a plausible seed, or accept a manual roster file. Fits the serving story well: "this symbol's only author left 8 months ago and it's a hotspot" is prime agent context. Effort M for main-author/bus-factor, L with loss risk. Note: blame is expensive — reuse the `GIT_BUDGET` pattern and restrict to hotspot files.

### 6. Trend reports per episode — **S, medium value**

Everything is already in the store (see (d)); add a `graph.trends` operation / report job: per-episode `node_count/edge_count` (gf_episodes), community continuity (`overlap`), symbol moves, coverage percentage (gf_test_coverage), hotspot churn. The jobs/report pipeline and the vision in architecture-concepts.md:103 ("coupling over time, violations per episode … emitted via the existing jobs/report pipeline") already describe this. Effort S.

## (d) CodeScene-style trends the bi-temporal model gives for free

- **Time travel queries**: `graph.as_of` with `valid_at`/`known_at`/`branch` (temporal_graph.rs:120-141, gf_timeline in temporal_schema.sql:2-8) is precisely "the code as known at date X" — CodeScene's per-date analyses need delta snapshots to do this; Graphicle has it natively.
- **Full episode history retained**: retention.rs trims only coverage/cross-edges/unresolved windows (20/20/2); every episode, node and edge window is kept — trend lines don't degrade.
- **Community evolution**: `gf_communities.previous_id/overlap` (and `(:Community)-[:CONTINUES {overlap}]->`, cypherlite-migration.md:101) is community-level trend analysis out of the box — "these two communities merged over the last 30 syncs" is one query.
- **Symbol genealogy**: `gf_symbol_moves` + episode-versioned `gf_nodes` windows = per-symbol change frequency (sync-churn) and rename history today, without a single git call.
- **Coverage trend**: `gf_test_coverage` per episode gives test-coverage-over-time joined to symbol spans.
- **Structural trend**: per-episode `node_count`/`edge_count`/contradictions/god-node drift are one GROUP BY away.

Two honest limits: (1) episodes are **working-tree syncs**, not commits — they batch many changes, arrive irregularly, and only exist since the project's first Graphicle sync; temporal coupling at episode granularity would be far too coarse, which is why item 1 (commit ingestion) is the prerequisite that matters. (2) Nothing today aggregates any of this into a *served* metric — the data is present, the reports are not.

## Sources

- [CodeScene Terminology](https://codescene.io/docs/terminology/codescene-terminology.html)
- [CodeScene — code health KPIs](https://codescene.com/blog/3-code-health-kpis/)
- [CodeScene — what is code health](https://codescene.com/blog/measure-code-health-of-your-codebase)
- Adam Tornhill, *Your Code as a Crime Scene* (principles behind hotspots/change coupling/knowledge analysis)
