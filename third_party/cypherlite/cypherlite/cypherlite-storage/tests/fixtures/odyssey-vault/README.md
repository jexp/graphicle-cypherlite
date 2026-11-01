# Odyssey fixture vault

Hand-authored markdown-mode fixture used by task-014/task-015/task-016. It
pairs a 20-concept markdown vault (this directory) with `import.cypher`,
which reproduces the exact same graph in Neo4j, so the renderer's output can
be diffed byte-for-byte against these files (task-015) or against a mounted
FUSE tree backed by a live database (task-016).

## Assumed `.lpgfs.yaml` config

Every fixture file was generated against this markdown-mode configuration
(`mode.markdown` fields left at their library defaults — see
`DEFAULT_MARKDOWN_MODE_CONFIG` in `src/types/index.ts` — plus an explicit
`labels` allow-list and a per-label `name`-property naming strategy):

```yaml
naming:
  default: elementId
  overrides:
    nodes:
      Character: { property: name }
      Place: { property: name }
      Creature: { property: name }
      Event: { property: name }

sanitization:
  replace:
    "/": "_"
    "\\": "_"
    "\0": "_"
    ":": "_"
    "*": "_"
    "?": "_"
    '"': "_"
    "<": "_"
    ">": "_"
    "|": "_"

collision:
  strategy: suffix_elementId

mode:
  type: markdown
  markdown:
    labels: [Character, Place, Creature, Event]
    linkStyle: wikilink
    includeIncoming: false
    textProperties:
      default: [summary, text, content]
    fields:
      title: [title, name]
      timestamp: [updated, lastUpdated, modified, created]
      tags: [tags, categories]
```

`mode.markdown.labels` is set explicitly (rather than left unset) so the
`Hero` label carried by Odysseus never mounts its own top-level directory —
it exists purely to exercise task-009's first-label-wins rule. Every other
field mirrors `DEFAULT_MARKDOWN_MODE_CONFIG` verbatim; task-015's round-trip
test must use this same config object.

## Vault contents

- 20 concept files across 4 labels: **Character** (7, including one
  multi-label node), **Place** (5), **Creature** (4), **Event** (4).
- `Character/Odysseus.md` — labels `[Character, Hero]`. Renders once under
  `Character/` per REQ-F-014/task-009 (first-label-wins); has both an
  explicit `title` and a scalar `tags` value (coerced to a one-item list);
  has two present text properties (`summary` and `content`), so its body
  renders with `## summary` / `## content` headings instead of bare text.
- `Character/Circe.md` — carries a literal `type: Sorceress` property,
  exercising task-013's clash rule: the label wins the `type` frontmatter
  key (`type: Character`) and the property is demoted to
  `type_property: Sorceress`.
- `Character/Eumaeus.md` — has no `title`/`timestamp`/`tags` source
  property and none of `summary`/`text`/`content` present, so it renders
  frontmatter-only (`title` falls back to `name`) with an empty body.
- Ten nodes have a resolvable timestamp, split across the `updated`,
  `lastUpdated`, `modified`, and `created` fallback properties and four
  distinct dates (one date — 2026-02-01 — shared by three nodes across two
  labels, to exercise `log.md`'s within-date tie-break sort by
  label-then-name); the remaining ten nodes have no timestamp property and
  are correctly absent from `log.md`.
- All relationships are authored one-directional (matching
  `includeIncoming: false`), so every relationship key in a `.md` file is an
  outgoing link; the reverse direction is never rendered as an `in_<TYPE>`
  key.

## How the expected index.md/log.md/*.md files were generated

Rather than hand-computing the exact YAML/Markdown byte-for-byte, the
fixtures were produced by a throwaway script
(not checked in — see task-014's progress note for the approach) that:

1. Built the same 20-node/30-relationship graph as plain JS objects.
2. Ran `npm run build` and imported the *compiled* `dist/markdown/renderer.js`,
   `dist/markdown/fields.js` (`propertyFallbackFieldResolver`),
   `dist/markdown/index-renderer.js`, `dist/markdown/log.js`, and the
   `chooseNodeLabel`/`groupRelationshipsForMarkdown`/`truncateDescription`
   helpers from `dist/db/queries.js` — i.e. the exact functions the FUSE
   handlers call in production, not a reimplementation.
3. For each node: resolved its rendered label via `chooseNodeLabel`,
   reordered its labels so the chosen one is first (mirroring
   `renderMarkdownNode` in `src/fuse/handlers.ts`), built relationship rows
   for both directions and ran them through `groupRelationshipsForMarkdown`,
   then called `renderNodeMarkdown` and wrote the result to
   `<Label>/<name>.md`.
4. Computed the title/timestamp/description projection each node would get
   from `listNodesForMarkdownIndex` (COALESCE over the same fallback lists,
   `truncateDescription` on the first present text property) and fed those
   rows into `renderLabelIndex`/`renderRootIndex`/`renderRootLog`.

This guarantees the fixtures match the renderer's real output rather than a
hand-traced approximation, which is what task-015's byte-for-byte test
needs.
