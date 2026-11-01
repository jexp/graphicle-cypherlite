# Published jQAssistant + kontext-e Git plugin queries — collected for the graphicle git schema

Sources checked (2026-10-30): softvis-research/jqa-dashboard (the "dashboard plugin packages the same rules" lead — confirmed), kontext-e/jqassistant-plugins (old monorepo), kontext-e/jqassistant-git-plugin (continuation repo), JohT/code-graph-analysis-pipeline, bernardo.gitbook.io jQA notes, StackOverflow 78194357, feststelltaste.de (jQA tag + "Software Archaeology with Git"). **Bottom line: genuinely published Cypher against the kontext-e `Git:` model is scarce — author dedup, a time tree, two flat exports, and one author-filtered file list. The only serious published change-coupling Cypher (JohT) targets its own `Git:Log` CSV model, not the kontext-e model. Bus factor, knowledge loss and rename-aware churn have no published jQA queries; ours below are marked.**

Verified kontext-e model (context for corrections):
`(:Git:Repository)-[:HAS_COMMIT]->(:Git:Commit {sha, author, committer, date, epoch, message, shortMessage})-[:CONTAINS_CHANGE]->(:Git:Change {modificationKind: A/M/D/R/C})-[:MODIFIES]->(:Git:File {relativePath, createdAt, deletedAt, lastModificationAt, *AtEpoch})`; `(:Git:Author {identString, name, email})-[:COMMITTED]->(:Git:Commit)`; `(:Git:Commit)-[:HAS_PARENT]->(:Git:Commit)`. Rename extras (git.adoc): `(:Git:Change)-[:RENAMES]->oldFile` plus `CREATES`/`DELETES`; `(:Git:File)-[:HAS_NEW_NAME]->(:Git:File)`, `COPY_OF`. Plugin ≥2.3.0 also labels Change nodes `:Create/:Update/:Deletes/:Rename/:Copy` and puts timestamps on the relations.

Our planned schema (all "ours" queries target this):
`(:Person {email, name})-[:AUTHORED]->(:Commit {oid, subject, author_date})-[:CONTAINS_CHANGE]->(:Change {kind: A/M/D/R/C})-[:MODIFIES]->(:File {path})`, anchored `(p:Project)-[:HAS_COMMIT]->(c:Commit)`. Assumptions flagged inline: `author_date` stored as datetime; `File.path` is the repo-relative path.

---

## A. Published queries (verbatim + correction)

### 1. Author dedup by email — softvis-research/jqa-dashboard, `data/jqassistant/dashboard.adoc`, concept `GitDuplicateAuthorsByEmail`
Source: https://github.com/softvis-research/jqa-dashboard/blob/master/data/jqassistant/dashboard.adoc
Verbatim:
```cypher
MATCH
  (a:Author)
WITH
  a.email as email, collect(a) as authors
WITH
  head(authors) as author, tail(authors) as duplicates
UNWIND
  duplicates as duplicate
MATCH
  (duplicate)-[:COMMITTED]->(c:Commit)
MERGE
  (author)-[:COMMITTED]->(c)
DETACH DELETE
  duplicate
RETURN
  author.email as Author, count(duplicate) as NumberOfDuplicates
ORDER BY
  NumberOfDuplicates desc
```
Correction vs. verified model: `(:Git:Author)`, not bare `:Author`; relation is `COMMITTED` (correct).
Ours: since `:Person` is keyed by `email`, dedup only matters for name aliases —
```cypher
// merge Person nodes that share an email, keeping distinct names as an alias set
MATCH (p:Person)
WITH p.email AS email, collect(p) AS people
WHERE size(people) > 1
WITH head(people) AS keeper, tail(people) AS dupes
UNWIND dupes AS dupe
MATCH (dupe)-[a:AUTHORED]->(c:Commit)
MERGE (keeper)-[:AUTHORED]->(c)
WITH keeper, collect(DISTINCT dupe.name) + keeper.name AS names
SET keeper.aliases = apoc.coll.toSet(names)
DETACH DELETE dupe
```
Mapping: `:Git:Author.email` → `:Person.email`; `COMMITTED` → `AUTHORED`.

### 2. Author dedup by name — same file, concept `GitDuplicateAuthorsByName`
Same source; identical body keyed on `a.name` instead of `a.email` (see URL above; only the first/last lines differ: `a.name as name … RETURN author.name`). Ours: the mirror of query 1 keyed on `name` — useful because git2 commits often carry different emails for one human. Both variants matter for us: Rust projects scanned from raw git2 data will have exactly this duplicate-person problem.

### 3. Time tree (churn-over-time scaffold) — jqa-dashboard, concept `GitTimeTree`
Source: same URL as above.
Verbatim:
```cypher
MATCH
  (c:Commit)
WITH
  c, split(c.date, "-") as parts
MERGE
  (y:Year{year:parts[0]})
MERGE
  (m:Month{month:parts[1]})-[:OF_YEAR]->(y)
MERGE
  (d:Day{day:parts[2]})-[:OF_MONTH]->(m)
MERGE
  (c)-[:OF_DAY]->(d)
RETURN
    y.year as Year, count(c) as Commits
ORDER BY
  Year
```
Correction vs. verified model: fine as-is on the kontext-e model (`date` is a string like `2024-01-30 ...`). Our schema stores a real datetime, so we don't need a materialized time tree — see query 11 (ours) for the date()-based churn-over-time version, which replaces this concept entirely.

### 4. Churn per file (change count) — bernardo.gitbook.io "Development Docs — Java", jQA/queries/git
Source: https://bernardo.gitbook.io/development-docs-java/jqassistant/queries/git
Verbatim:
```cypher
MATCH
   (change:Git:Change),
   (change)-[]->(file:File)
RETURN DISTINCT
   file.fileName AS file,
   count(change) AS changes
```
Corrections vs. verified model: the undirected `)-[]->(file:File)` should be `(:Git:Change)-[:MODIFIES]->(file:Git:File)`; `fileName` only exists after the dashboard's `GitFileName` concept ran — use `relativePath` directly; and anchor to a project. Notable: this counts *change entries*, so a file touched twice in one commit counts twice.
Ours:
```cypher
MATCH (p:Project)-[:HAS_COMMIT]->(:Commit)-[:CONTAINS_CHANGE]->(:Change)-[:MODIFIES]->(f:File)
RETURN f.path AS file, count(*) AS changes
ORDER BY changes DESC
```
Mapping: `:Git:File.relativePath` → `:File.path`; `CONTAINS_CHANGE`/`MODIFIES` unchanged.

### 5. Flat git-history export — same gitbook page
Source: same URL.
Verbatim:
```cypher
MATCH
   (commit:Git:Commit),
   (commit)-[:CONTAINS_CHANGE]->(change:Git:Change),
   (author:Git:Author)-[:COMMITTED]->(commit),
   (change)-[]->(file:File)
OPTIONAL MATCH
   (class)-[:HAS_SOURCE]->(file:Git:File),
   (package:Package)-[:CONTAINS]->(class)
RETURN DISTINCT
   commit.sha AS sha,
   commit.date AS date,
   commit.time AS time,
   commit.author AS author,
   author.email AS author_email,
   author.identString AS author_id,
   commit.committer AS commiter,
   commit.message AS message,
   change.modificationKind AS modificationKind,
   file.fileName AS file,
   file.type AS fileType,
   class.name AS class,
   package.fileName AS package
```
Corrections vs. verified model: `commit.author` is the raw ident string (keep or drop); `commit.time` and `file.type` are dashboard-concept products, not plugin-native — drop or compute first; the `OPTIONAL MATCH` is a comma-separated pattern that only makes sense with the Java plugin loaded; again `)-[]->(file)` should be `[:MODIFIES]`. Value for us: this is the canonical "join commit-author-change-file into one table" shape that every downstream analysis (and our CSV/report export) needs.
Ours:
```cypher
MATCH (p:Project)-[:HAS_COMMIT]->(c:Commit)-[:CONTAINS_CHANGE]->(ch:Change)-[:MODIFIES]->(f:File),
      (a:Person)-[:AUTHORED]->(c)
RETURN c.oid AS oid, a.email AS author_email, c.author_date AS date,
       c.subject AS subject, ch.kind AS kind, f.path AS file
ORDER BY c.author_date
```
Mapping: `sha`→`oid`, `shortMessage`→`subject`, `message`→ body (not in our schema yet), `Git:Author`→`:Person`, `COMMITTED`→`AUTHORED`, `date/epoch`→`author_date` (datetime — no separate epoch needed).

### 6. Files changed by a given author — StackOverflow 78194357, answer
Source: https://stackoverflow.com/questions/78194357/using-jqassistant-with-git-plugin-to-determine-changed-files-by-author
Verbatim (answer; the question asked why its attempt returned duplicates):
```cypher
MATCH (author:Git:Author)-[:COMMITTED]->(:Git:Commit)-[:CONTAINS_CHANGE]->(:Git:Change)-->(file:Git:File)
WHERE author.email="developer-of-interest@email-address.com"
WITH DISTINCT file
RETURN file.relativePath
ORDER BY file.relativePath ASC
```
Correction vs. verified model: valid as-is (the `-->` is `MODIFIES` in practice; the answer itself recommends naming the relation type explicitly).
Ours:
```cypher
MATCH (p:Project)-[:HAS_COMMIT]->(:Commit)-[:CONTAINS_CHANGE]->(:Change)-[:MODIFIES]->(f:File),
      (a:Person {email: $email})-[:AUTHORED]->(c)
RETURN DISTINCT f.path AS file
ORDER BY file
```

### 7. Co-change coupling (temporal coupling) — JohT/code-graph-analysis-pipeline, `domains/git-history/queries/enrichment/Add_CHANGED_TOGETHER_WITH_relationships_to_git_log_files.cypher`
Source: https://github.com/JohT/code-graph-analysis-pipeline/blob/main/domains/git-history/queries/enrichment/Add_CHANGED_TOGETHER_WITH_relationships_to_git_log_files.cypher
This is the best published change-coupling Cypher anywhere in the jQA ecosystem. It targets JohT's own CSV-import model (`(:Git:Log:Commit {isManualCommit, isMergeCommit})-[:CONTAINS_CHANGED]->(:Git:Log:File {fileName, updateCommitCount})`), **not** the kontext-e model. Verbatim, core (the file's final `RETURN` is a ~40-line stats block of percentile rows over the same variables — omitted here, present at the URL):
```cypher
// Determine global file count, global file count threshold (filter out refactoring commits) and global update commits
MATCH (git_commit_global:Git:Log:Commit)-[:CONTAINS_CHANGED]->(git_file_global:Git:Log:File)
WHERE git_commit_global.isManualCommit
  AND NOT git_commit_global.isMergeCommit
 WITH git_commit_global, count(DISTINCT git_file_global) AS commitFileCount
 WITH percentileDisc(commitFileCount, 0.95) AS globalFileCountThreshold
     ,count(git_commit_global)              AS globalUpdateCommitCount
// Main section
MATCH (git_commit:Git:Log:Commit)-[:CONTAINS_CHANGED]->(git_file:Git:Log:File)
WHERE git_commit.isManualCommit
  AND NOT git_commit.isMergeCommit
MATCH (git_repository:Git&Repository)-[:HAS_FILE]->(git_file)
// Order files to assure, that pairs of distinct files are grouped together (fileA, fileB) without (fileB, fileA)
ORDER BY git_commit.hash, git_file.fileName
 WITH globalFileCountThreshold
     ,globalUpdateCommitCount
     ,git_commit.hash             AS commitHash
     ,collect(DISTINCT git_file)  AS filesInCommit
// Limit the file count to min. 2 (changed together) and
// max. 50 (reduce permutations, improve performance, filter out large refactorings that usually affect many files)
WHERE size(filesInCommit) >= 2
  AND size(filesInCommit) <= globalFileCountThreshold
// Collect distinct pairwise (..., 2, 2) combinations of all files in the list
 WITH globalFileCountThreshold
     ,globalUpdateCommitCount
     ,commitHash
     ,apoc.coll.combinations(filesInCommit, 2, 2) AS fileCombinations
UNWIND fileCombinations AS fileCombination
  WITH globalFileCountThreshold
      ,globalUpdateCommitCount
      ,fileCombination
      ,count(DISTINCT commitHash) AS updateCommitCount
// Filter out file pairs that weren't changed very often together
WHERE updateCommitCount > 2
 WITH *
     ,fileCombination[0] AS firstFile
     ,fileCombination[1] AS secondFile
 WHERE firstFile <> secondFile
 WITH *
     // Get the lowest number of git update commits of both files (file pair)
     ,CASE WHEN firstFile.updateCommitCount < secondFile.updateCommitCount
           THEN firstFile.updateCommitCount
           ELSE secondFile.updateCommitCount
      END AS minUpdateCommitCount
      // Calculate update commit support by dividing the update commit count by the overall commit count for both files
     ,toFloat(firstFile.updateCommitCount)  / globalUpdateCommitCount AS firstFileUpdateSupport
     ,toFloat(secondFile.updateCommitCount) / globalUpdateCommitCount AS secondFileUpdateSupport
 WITH *
      // Expected likelihood that the first and the second file change together given complete randomness
     ,firstFileUpdateSupport * secondFileUpdateSupport AS expectedCoUpdateSupport
 WITH firstFile
     ,secondFile
     ,updateCommitCount
     // Out of all the times the less frequently changed file was touched, how often did it co-occur with the other file?
     ,toFloat(updateCommitCount) / minUpdateCommitCount    AS updateCommitMinConfidence
     // Compared to all commits in general, how high is the percentage of the commits where both files changed together?
     ,toFloat(updateCommitCount) / globalUpdateCommitCount AS updateCommitSupport
     // Lift
     ,toFloat(updateCommitCount) / (globalUpdateCommitCount * expectedCoUpdateSupport) AS updateCommitLift
     // Jaccard Similarity: Of all commits involving either file, how many involved both?
     ,toFloat(updateCommitCount) / (firstFile.updateCommitCount + secondFile.updateCommitCount - updateCommitCount) AS updateCommitJaccardSimilarity
 CALL (firstFile, secondFile, updateCommitCount, updateCommitMinConfidence, updateCommitSupport, updateCommitLift, updateCommitJaccardSimilarity) {
       MERGE (firstFile)-[pairwiseChange:CHANGED_TOGETHER_WITH]-(secondFile)
         SET pairwiseChange.updateCommitCount             = toInteger(updateCommitCount)
            ,pairwiseChange.updateCommitMinConfidence     = updateCommitMinConfidence
            ,pairwiseChange.updateCommitSupport           = updateCommitSupport
            ,pairwiseChange.updateCommitLift              = updateCommitLift
            ,pairwiseChange.updateCommitJaccardSimilarity = updateCommitJaccardSimilarity
       } IN TRANSACTIONS OF 500 ROWS
```
Design choices worth stealing wholesale: cap commit size at the 95th percentile (kills "refactoring commits" that couple everything to everything); require `> 2` co-changes; order files within a commit before pairing so `(a,b)` and `(b,a)` group together; store minConfidence / support / lift / jaccard on the edge so ranking is cheap later. `apoc.coll.combinations` and `CALL {} IN TRANSACTIONS` need APOC / Neo4j 5+ — for CypherLite we must inline the pairing (see ours).
Ours (schema above; pairing done with a self-join instead of APOC):
```cypher
MATCH (p:Project)-[:HAS_COMMIT]->(c:Commit)-[:CONTAINS_CHANGE]->(:Change)-[:MODIFIES]->(f:File)
WITH c, count(f) AS filesInCommit
WHERE filesInCommit >= 2 AND filesInCommit <= 50
MATCH (c)-[:CONTAINS_CHANGE]->(:Change)-[:MODIFIES]->(f1:File), (c)-[:CONTAINS_CHANGE]->(:Change)-[:MODIFIES]->(f2:File)
WHERE f1.path < f2.path                       // canonical pair ordering, replaces ORDER BY + combinations
WITH f1, f2, count(DISTINCT c) AS coChanges
WHERE coChanges > 2
MATCH (f1)<-[:MODIFIES]-(:Change)<-[:CONTAINS_CHANGE]-(:Commit)-[:CONTAINS_CHANGE]->(:Change)-[:MODIFIES]->(f2)  // noop pattern drop-in if per-file totals precomputed
WITH f1, f2, coChanges,
     CASE WHEN f1.totalChanges < f2.totalChanges THEN f1.totalChanges ELSE f2.totalChanges END AS minChanges
SET ...  // (f1)-[:CHANGED_TOGETHER_WITH {count: coChanges,
       //  confidence: toFloat(coChanges)/minChanges}]-(f2)  — see note
```
Practical note: precompute per-file totals once (`MATCH (f:File)<-[:MODIFIES]-(:Change)<-[:CONTAINS_CHANGE]-(:Commit {project: $pid}) SET f.totalChanges = count(*)`) — then the pairwise MERGE is a two-pass query. Mapping: `CONTAINS_CHANGED`→`CONTAINS_CHANGE`, `Git:Log:File.fileName`→`File.path`, commit id `hash`→`oid`; `isManualCommit/isMergeCommit` have no equivalent in our schema yet — we should either skip merge commits at scan time or store `:Merge` (dashboard concept 8 below) and filter `NOT c:Merge`.

### 8. Merge-commit labeling — jqa-dashboard, concept `GitMergeCommit`
Source: same dashboard.adoc URL.
Verbatim:
```cypher
MATCH
  (c:Commit)-[:HAS_PARENT]->(p:Commit)
WITH
  c, count(p) as parents
WHERE
  parents > 1
SET
  c:Merge
RETURN
  count(c) as MergeCommits
```
Correction vs. verified model: `:Git:Commit` labels; otherwise valid. Ours: same shape on `(:Commit)-[:HAS_PARENT]->(:Commit)` (add `HAS_PARENT` to our schema — it's in the verified model and we need it for this filter and for first-parent walking). Every churn/coupling query above should exclude `c:Merge`.

### 9. Branch-merge reachability (context only) — kontext-e/jqassistant-plugins, `git/src/main/asciidoc/git.adoc`, "Git Sample Queries"
Source: https://github.com/kontext-e/jqassistant-plugins/blob/master/git/src/main/asciidoc/git.adoc (same content in the continuation repo https://github.com/kontext-e/jqassistant-git-plugin/blob/main/src/main/asciidoc/git.adoc)
Verbatim (the only sample query the plugin itself publishes):
```cypher
MATCH
    (git:Git)
      // Any branch
      --> (branch:GitBranch)
      -[:HEAD]-> (branchHead:Git:Commit)
      <-[r:HAS_PARENT*]-(developHead:Git:Commit)
      // The develop branch
      <-[:HEAD]- (develop:GitBranch {name: "remotes/origin/develop"})
WHERE branch.name =~ "remotes/origin/release/.*"
RETURN * // git, develop, branch
```
Stale vs. verified model: `GitBranch`/`HEAD`/`(git:Git)` are pre-migration names — today it's `(:Git:Repository)-[:HAS_BRANCH]->(:Git:Branch {name})-[:HAS_HEAD]->(:Git:Commit)` and `[:HAS_PARENT*]` survives unchanged. Off-topic for author/coupling, but the variable-length `[:HAS_PARENT*]` walking pattern is the one reusable trick. Our schema has no branches — note this as an explicit non-goal or a later addition.

Also from jqa-dashboard (minor scaffolding, kept as notes not queries): `GitFileName` copies `relativePath`→`fileName` for indexing (irrelevant for us — index `path` directly); `FileType` splits the extension out of `relativePath` into `f.type` (useful for churn-by-file-type; one `split()` in our query layer); `TypeHasSourceGitFile` joins `:Java:Type` to `:Git:File` by path suffix — the pattern to copy when we join graphicle's code nodes to git `:File` nodes (join on exact path, not suffix, since we control both sides).

---

## B. Missing from the literature — ours (no published jQA equivalent found)

### 10. OURS — file ownership + bus factor
```cypher
MATCH (p:Project)-[:HAS_COMMIT]->(c:Commit)-[:CONTAINS_CHANGE]->(:Change {kind: 'M'})-[:MODIFIES]->(f:File),
      (a:Person)-[:AUTHORED]->(c)
WITH f, a, count(*) AS touches
WITH f, sum(touches) AS total, max(touches) AS top, collect({a: a.email, t: touches}) AS contributors
RETURN f.path AS file, total,
       [x IN contributors WHERE x.t = top][0].a AS dominantAuthor,
       toFloat(top)/total AS ownership,
       size(contributors) AS contributors
ORDER BY ownership DESC, total DESC
```
Bus-factor read: `size(contributors) WHERE ownership > 0.5` = files one person owns; count them per top-level directory for a project-level truck factor. Restrict to kind 'M' (or A+M) so renames don't inflate.

### 11. OURS — churn over time (replaces the dashboard's time tree)
```cypher
MATCH (p:Project)-[:HAS_COMMIT]->(c:Commit)
WHERE NOT c:Merge
RETURN date(c.author_date).year AS year, date(c.author_date).month AS month,
       count(c) AS commits, count(DISTINCT c.subject) AS subjects
ORDER BY year, month
```
Per-file monthly churn: add `-[:CONTAINS_CHANGE]->(:Change)-[:MODIFIES]->(f:File)` and group by `f.path` too (resolve renames first via query 13 or old paths will show as "dying").

### 12. OURS — knowledge loss / departed experts
```cypher
MATCH (p:Project)-[:HAS_COMMIT]->(c:Commit)-[:CONTAINS_CHANGE]->(:Change)-[:MODIFIES]->(f:File),
      (a:Person)-[:AUTHORED]->(c)
WITH f, a, count(*) AS touches, max(c.author_date) AS lastTouch
WITH f, a, touches, lastTouch, duration.inDays(date(lastTouch), date()).days AS daysSince
WITH f, a, touches,
     CASE WHEN touches >= 3 AND daysSince > 365 THEN touches ELSE 0 END AS lostKnowledge
RETURN a.name AS departedContributor, a.email,
       sum(lostKnowledge) AS orphanedTouches, count(DISTINCT f) AS filesTouched
ORDER BY orphanedTouches DESC
```
Stricter variant (matches the classic knowledge-loss metric): count only files whose *current* content the departed author last touched — require `lastTouch = max(all touches on f)` via a per-file max pass. Thresholds (3 touches, 365d) from the turnover-knowledge-loss literature (e.g. ICSME 2017), tune per repo activity.

### 13. OURS — rename-aware churn (HAS_NEW_NAME)
Kontext-e models a rename as old file `-[:HAS_NEW_NAME]->` new file plus a Change `{modificationKind:'R'}` with `RENAMES`→old. Our schema needs the same `(:File)-[:HAS_NEW_NAME]->(:File)` edge (proposed addition, mirroring kontext-e) or `:Change {kind:'R'}` carrying both endpoints. Canonical current-path resolution:
```cypher
MATCH (f:File)
WHERE NOT ()-[:HAS_NEW_NAME]->(f)          // f is nobody's old name → still current
MATCH (f)<-[:MODIFIES]-(:Change)<-[:CONTAINS_CHANGE]-(c:Commit)
// walk rename chain backwards from f's history
OPTIONAL MATCH (prev)-[:HAS_NEW_NAME*]->(f)
RETURN f.path AS currentPath, count(DISTINCT c) + count(DISTINCT prev) AS lifetimeChanges
```
Simpler and cheaper at load time: when ingesting a `R` change, write the change against the *new* path and set `change.oldPath`; then churn queries are rename-proof by construction and no graph walking is needed. Recommend the load-time option for CypherLite.

---

## Source index
- jqa-dashboard rules: https://github.com/softvis-research/jqa-dashboard/blob/master/data/jqassistant/dashboard.adoc (repo: https://github.com/softvis-research/jqa-dashboard)
- kontext-e monorepo git.adoc (model + 1 sample): https://github.com/kontext-e/jqassistant-plugins/blob/master/git/src/main/asciidoc/git.adoc
- kontext-e continuation repo + changelog (rename/copy labels, timestamps on relations): https://github.com/kontext-e/jqassistant-git-plugin
- gitbook jQA git queries (2 queries): https://bernardo.gitbook.io/development-docs-java/jqassistant/queries/git
- SO changed-files-by-author: https://stackoverflow.com/questions/78194357/using-jqassistant-with-git-plugin-to-determine-changed-files-by-author
- JohT change coupling (CHANGED_TOGETHER_WITH + metrics): https://github.com/JohT/code-graph-analysis-pipeline/blob/main/domains/git-history/queries/enrichment/Add_CHANGED_TOGETHER_WITH_relationships_to_git_log_files.cypher
- feststelltaste jQA query examples (no git queries): https://www.feststelltaste.de/jqassistant-cypher-query-examples/ ; his git work is git-CLI based: https://www.feststelltaste.de/software-archaeology-with-git/
- Not fetched (large): the softvis-research "Unified Data Source" paper (easychair.org/publications/preprint/893N/open) describes the dashboard stack behind source 1.
