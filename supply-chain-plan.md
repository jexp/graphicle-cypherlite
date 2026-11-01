# Supply-chain plan — packages, versions, licenses and vulnerabilities in the graph

_2026-10-30. Companions: `research/guac-integration.md` (the GUAC assessment this builds on), `git-to-graph-plan.md`, `language-coverage-plan.md` (§1.2/§1.4 the manifest gaps). Verified against the live deps.dev v3 API (2026-10-30, anonymous), api.osv.dev, and the GUAC source (`pkg/handler/collector/deps_dev`)._

## 0. The decision this plan implements

Graphicle's external imports stop at version-less stubs and the engine drops external dependency edges entirely — the supply chain is invisible to agents. The GUAC research concluded: adopt **GUAC's ontology shape, not its infrastructure** (Go/NATS/GraphQL stack contradicts single-binary local-first). The user's addition — "we can import from deps.dev ourselves, and if GUAC does it well we can utilize their work for now" — resolves to: **a native deps.dev + OSV client in Rust, borrowing GUAC's purl mapping rules, field-selection discipline and throttling posture** (verified: plain REST/JSON, no auth — a reqwest client is a day's work). Running GUAC as a subprocess is rejected for the same reason the GUAC report gave; an optional *bridge* stays open for teams already running it (§5 P4).

One verified finding that shapes the whole split: **GUAC itself does not treat deps.dev as a replacement for OSV, licenses or Scorecard channels** — its deps.dev collector takes only resolved dependencies + source-repo links + Scorecard; advisories come from a separate osv.dev collector and licenses from ClearlyDefined. We follow the same split, with the simplification that deps.dev *does* return licenses and advisory-keys per version, so one client covers most of it for Rust (§1).

## 1. Data sources, verified

**deps.dev v3 REST** (`https://api.deps.dev/v3/...`, no auth, `cache-control: max-age=3600`, terms explicitly permit caching — plan an HTTP cache ≥1h TTL and modest concurrency):
- `GET /systems/{system}/packages/{name}` — all versions, `publishedAt`, `isDefault`/`isDeprecated`(+reason).
- `GET .../versions/{version}` — `licenses[]` (SPDX expressions), **advisoryKeys (OSV ids) for that exact version**, `links[SOURCE_REPO/HOMEPAGE/DOCUMENTATION]`, SLSA provenances, attestations.
- `GET .../versions/{version}:dependencies` — **resolved** transitive graph (`nodes` SELF/DIRECT/INDIRECT + `edges` with requirements) — **Cargo, npm, Maven, PyPI only**.
- `GET .../versions/{version}:requirements` — manifest-level constraints (cargo: requirement/kind/optional/features/target).
- `GET /projects/{repo-path}` — stars/forks, license, description, **full OpenSSF Scorecard**.
- `GET /advisories/{osv-id}` — title, CVE aliases, CVSS3 — but **no affected/fixed ranges**.

Constraints to design around: **no purl support anywhere** (decompose ourselves, §2), **no dependents** (BigQuery only — out of scope), **no resolved graphs for NuGet/RubyGems/Go** (Go requirements only), rate limits undocumented (GUAC self-throttles — behavioral evidence they bite).

**OSV** (`https://api.osv.dev/v1/query`): bulk-query with the whole `Cargo.lock` (or per-package `package@version` queries) — the better primitive for *affected ranges and fixed versions*, which deps.dev lacks. For a Rust project this is one call per lockfile.

**Local, offline sources** (no network, first phase): `Cargo.lock` via the `cargo-lock` crate (exact versions, features, sources — git deps too); `package.json`/lockfiles per `language-coverage-plan.md` §1.2; our own `manifest_ingest` already parses `Cargo.toml`/`pyproject.toml`/`go.mod`/`pom.xml` names.

## 2. Graph model

GUAC-shaped, keyed on a purl we mint ourselves (deps.dev won't parse one, but our keys should still *be* purls for future SBOM interop; the purl→VersionKey decomposition follows GUAC's rules: `golang`→`GO`, upper-case system otherwise, maven `groupId:artifactId`, skip versionless purls):

```
(:Package {purl, system, name}) -[:VERSION {version, is_default, is_deprecated, published_at}]-> (:PackageVersion {purl_at_version})
(:PackageVersion)-[:REQUIRES {requirement, kind, optional, features, target, origin}]->(:PackageVersion)   -- resolved or manifest-level, origin-tagged
(:PackageVersion)-[:LICENSED_AS {expression}]->(:License {spdx})
(:PackageVersion)-[:AFFECTED_BY {osv_id, cvss3, source:'osv', known_at}]->(:Advisory {osv_id, title, aliases, url})
(:PackageVersion)-[:SCANNED_CLEAN {as_of, source:'osv'}] ...          -- negative evidence (GUAC's NoVuln discipline)
(:PackageVersion)-[:FROM_PROJECT {stars, score}]->(:Project {repo_url})  -- deps.dev project + Scorecard
(:Symbol)-[:RESOLVES_TO]->(:Package)                                  -- the seam: an import resolves to a real package
(:File {path: 'Cargo.lock'}) -[:DESCRIBES]->(:PackageVersion)          -- provenance: which lockfile said so
```

Notes:
- `REQUIRES` (not `DEPENDS_ON`) for the package graph: the name `DEPENDS_ON` is already the invalidation domain's `(:Artifact)-[:DEPENDS_ON]->(:Source)`; endpoints are label-disjoint so queries wouldn't collide, but two meanings under one type invites confusion — rename once, here, while it's cheap.
- Everything is **anchored through the project** and provenance-tagged (`origin`: `cargo.lock`/`deps.dev`/`osv`/`sbom`; `known_at`: when we learned it) — the same discipline as `git-to-graph-plan.md` §2, and GUAC's collector/justification model.
- `AFFECTED_BY` + `SCANNED_CLEAN` mirror GUAC's negative-evidence pattern, which maps naturally onto our `UnresolvedCross`/hints machinery: "no advisory found *as of date*" is a fact, not an absence.

## 3. The seam: symbols ↔ packages

The `(:Symbol)-[:RESOLVES_TO]->(:Package)` edge is where Graphicle beats both GUAC (no code graph) and plain SBOM tooling (no source). Resolution rules per ecosystem, starting with Rust (all local, no network): `use serde::json` + Cargo.lock ⇒ the `serde` symbol's import edge resolves to `:Package pkg:cargo/serde` at the locked version. Add Rust to the cross-repo linker's manifest list (it already builds `:Package` nodes for Python/Node/Go via `manifest_ingest` + the projection's export index — the GUAC report verified Cargo.toml is parsed but produces identity-only nodes with no versions). Then "which of *our* files are affected by RUSTSEC-xxxx" becomes a 2-hop query: `(:Advisory)<-[:AFFECTED_BY]-(:PackageVersion)<-[:VERSION]-(:Package)<-[:RESOLVES_TO]-(:Symbol)-[:CONTAINS*]->(:File)`.

## 4. Mechanics

- **P1 offline** (in the sync job, no network): parse `Cargo.lock` → `:Package`/`:PackageVersion`/`REQUIRES` + `RESOLVES_TO` resolution. Idempotent MERGE on purl keys; the lockfile's `DESCRIBES` edge makes re-locks natural (new versions arrive as new nodes; old ones stay, un-referenced, until a retention pass).
- **P2 enrichment: on the fly, not materialized by default.** deps.dev is a *read-through* source: a fetch happens when an analysis, visualization, pack or MCP answer actually needs licenses/Scorecard/source-repo/deprecation, served from an HTTP cache (≥1h TTL, per the API's own cache-control) and an in-process LRU — **not** an eager job that re-materializes the ecosystem into the store. The store grows only by *pinning*: when an analysis's result must stay citable and reproducible (a report, a CI verdict, a `vuln_impact` answer with evidence), the facts it consumed are written with `as_of`/`known_at` and `origin` provenance — so a pinned analysis is re-runnable against the pinned data, and nothing else accumulates. The one eager exception is **OSV**: one bulk query per lockfile digest is cheap and security-relevant, so it stays a small job (still opt-out for fully-offline stores). `input_hash` = (lockfile digest, window config); GUAC-style self-throttle on every fetch.
- **P3 surfaces**: MCP tools (`deps_of(symbol|file)`, `vuln_impact(advisory|package)` reusing `get_pr_impact`'s output shapes, `license_of`), and pack integration — a pack's manifest can list "dependencies of the changed files' packages" the way it lists tests. Warnings-as-hints at edit time, in CI's voice only in the `analyze` gate (same two-surface rule as architecture-concepts §2).

### 4.1 Store topology: separate databases, merged on demand

The dependency graph is unbounded (the transitive closure of a lockfile is ecosystem-scale, not repo-scale) — it must not live in the project's store. CypherLite makes databases cheap, so:

- **Each project store keeps only its own lockfile facts** (`:Package`/`:PackageVersion`/`REQUIRES`/`RESOLVES_TO` — bounded by the lockfile, offline, §P1). Pinned analyses' evidence stays with the project (it is cited, so it goes where the receipts are).
- **Larger contexts — a full transitive graph, CVE/OSINT corpora, other registries — live in their own databases** (per-ecosystem, per-corpus), built on demand and cached on disk, never merged into the project store. A repo-set database (one set of repos *without forks*, deduped) keeps the **per-repo identity hashes honest**, exactly as `git-to-graph-plan.md` §5 argues for history.
- **Analysis merges are ephemeral**: when a visualization or impact analysis needs project + transitive deps + advisories together, they are cloned into a `:memory:` (or scratch-directory) analysis database and thrown away after — the pattern `graph_materialize` and expansion already use. Live tenants are never written into by an analysis, so a viz never blocks a sync.
- The Neo4j equivalent of this topology is **multidb** — per-context Neo4j databases selected per connection; see `neo4j-backend-plan.md`.

## 5. Feed for gbuild (node sketch)

- **`cargo-lock-ingest`** (S): lockfile → §2 package nodes, offline, idempotent. Acceptance: this repo's Cargo.lock round-trips; re-lock adds versions, never duplicates.
- **`rust-symbol-resolution`** (M): `RESOLVES_TO` for Rust imports (+ Rust in the cross-repo linker's manifest list). Acceptance: `use serde::...` in this repo resolves; a crates.io-missing crate lands as `:UnresolvedCross`-style negative evidence.
- **`deps-dev-client`** (M): native REST client (GetPackage/GetVersion/GetRequirements/GetDependencies/GetProject/advisories), purl→VersionKey per GUAC rules, cache + throttle. Acceptance: golden-file tests against recorded responses; no network in CI.
- **`osv-bulk-query`** (S): lockfile batch query → `AFFECTED_BY`/`SCANNED_CLEAN` with ranges. Acceptance: a known RUSTSEC advisory (e.g. the `time` advisory) flags the exact version and names the fixed version.
- **`supply-chain-job`** (M): the on-the-fly resolution layer — deps.dev read-through + cache + throttle, OSV bulk per lockfile digest, and the *pin* write path (provenance + `as_of`) for citable analyses. No eager deps.dev materialization anywhere. Acceptance: a fully-offline store syncs unchanged; `vuln_impact` on demand populates only the evidence it cites; a pinned report re-runs against pinned `as_of` data.
- **`mcp-package-tools`** (S/M): §4 P3 tools + pack manifest section. Acceptance: `vuln_impact` on a synthetic advisory returns cited `path:line` evidence.
- **Deferred, decided against for now**: SBOM import (CycloneDX/SPDX) and a GUAC bridge — P4, only when a team actually runs GUAC beside Graphicle; clearlydefined license certifier (deps.dev licenses suffice for Rust); dependents via BigQuery.

Sequencing: `cargo-lock-ingest` → `rust-symbol-resolution` → `osv-bulk-query` → `deps-dev-client` → `supply-chain-job` → tools. P1+OSV alone already answers "is anything in my lockfile vulnerable, and where do we use it" with zero third-party infrastructure; deps.dev stays lazy throughout — fetched when an analysis asks, pinned only when an answer must outlive the question.
