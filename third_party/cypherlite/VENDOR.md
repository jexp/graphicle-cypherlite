# Vendored CypherLite engine

This directory is a vendored snapshot of the CypherLite repository
(https://github.com/neo4j-labs/cypherlite — a Neo4j Labs project, access
restricted), taken at commit `221329d5269f8e5dcc17a092ca39d2c4716d51e1`
(v0.13.0 + 51 commits, 2026-10-29). It is the engine Graphicle embeds as
its graph store — see `crates/graphicle/src/store_graph.rs` and
`cypherlite-migration.md` in the Graphicle repository.

## What is included

- `cypherlite/` — the engine crate group in full: the `cypherlite` facade
  plus model, parser, error, splitter, core, catalog, ir, front, planner,
  executor, ontology and storage.
- `testing/cypherlite-test-support/` — the shared test helpers the engine
  crates dev-depend on.
- The workspace manifest, the lock, the toolchain pin, the README, the
  CHANGELOG, the Labs disclaimer and the contribution guide, for
  provenance.

## What is not included

`surfaces/` (CLI, servers, language bindings), `clients/`, `docs/`,
`testing/` beyond test-support, `testdata/`, `baselines/`, `examples/`,
`scripts/`, `distribution/`, images and container files. None of them are
needed to build or test the engine crates; the workspace manifest was
patched accordingly (members and default-members pruned to what is here).

The manifest patch is the ONLY modification to the vendored source. To
update the snapshot: refresh from a checkout of the upstream repository at
the new revision, re-apply the members patch, and update the rev in this
file.

## License

Apache-2.0 (the upstream repository's ADR-0055; the full text is in
`LICENSE`). The Neo4j Labs experimental disclaimer (`LABS_DISCLAIMER.txt`)
applies: pre-1.0, not for production, no support. Graphicle's `NOTICE.md`
carries the attribution.
