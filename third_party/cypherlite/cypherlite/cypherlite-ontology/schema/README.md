# Vendored Ontology Graph Spec files

Pinned from `neo4j/graph-spec` @
`066515593ae59317d758bcf2cef54195e74bde39` (main as of 2026-10-19; see
ADR-0364 for why the schema is vendored and ADR-0227 §6 for the original
vendoring decision).

- `ontology-graph-spec.schema.json` — the JSON Schema (2020-12) for the
  main Ontology Graph Spec v1. Its `$id` is
  `https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json`, the only
  value the root `schema` gate accepts (ADR-0364 §1). The validator in
  this crate is hand-rolled structural checking of exactly this schema;
  the file is vendored as the human-readable source of truth and for
  fixture comparisons.
- `example.json` — a **CypherLite-authored** document in the main shape,
  used as a validation fixture in this crate's tests. It is validation
  only: it deliberately contains main-valid constructs that
  `db.ontology.apply` rejects, so it pins the accepted schema rather than
  convertibility (ADR-0364 §6). It covers every `Neo4jType` token, all
  four constraint types, the nullable/empty forms and every typed
  extension member.

Main ships **no root-level `example.json`**: the pinned README's curl URLs
are stale. Upstream's `go/model/testdata/graph-spec-example.json` is
main-shaped but does not validate against main's own schema —
`relationships.ACTED_IN.from.property` and `.to.property` are extra
members that `RelationshipTarget`'s closed shape rejects. It is not
vendored: verbatim it is invalid, and stripped it duplicates coverage the
authored example already provides.

Fetch command used to vendor the schema:

```sh
curl https://raw.githubusercontent.com/neo4j/graph-spec/066515593ae59317d758bcf2cef54195e74bde39/ontology-graph-spec.schema.json
```

Bumping the supported spec version is a new ADR: a new vendored schema, a
validator update, and a raised gate (ADR-0364 §1).
