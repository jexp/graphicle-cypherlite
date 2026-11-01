# Software visualization for Graphicle — a curated survey and dashboard series

_2026-10-29. Scope: interactive dashboards on top of Graphicle's (post-migration) CypherLite backend, delivered as a local webpage and inside VS Code. Companions: `../architecture-concepts.md` (§3 sketched cartography first), `../cypherlite-migration.md` (§2 graph model), `../findings.md`._

Attributions below were verified against web sources where stated; anything uncertain is marked explicitly. Local facts (Graphicle's schema, cypherlite's NVL usage) come from this checkout and the sibling `../cypherlite` checkout.

## 0. What a dashboard can query — Graphicle's graph model in one screen

Everything below already exists or is firmly planned (`cypherlite-migration.md` §2.4–2.5, `architecture-concepts.md` §6):

| Facet | Nodes / rels | Useful properties for viz |
|---|---|---|
| Code structure | `(:Symbol)` with `(:SymbolVersion:Live)`; `CONTAINS`, `IMPORTS`, `CALLS`, `INHERITS`, `USES` (live) and `*_HIST` (closed, carrying `valid_from_episode`/`valid_to_episode`) | `kind`, `qualified_name`, `relpath`, provenance `relpath`/`source_location` on edges |
| Files & modules | `(:File)-[:HEAD]->(:Object)`, `(:Chunk)`; modules via `CONTAINS` rollup | CAS content, chunk text |
| Episodes (change history) | `(:Episode)-[:FOLLOWS]->(:Episode)`; bi-temporal `*_HIST` windows; `(:Commit)-[:PARENT {position}]->(:Commit)`, `(:Assertion)-[:ON_TIMELINE]->` | `episode`, `sequence`, `valid_us`, `recorded_us` |
| Communities | `(:Community)-[:HAS_MEMBER]->(:Symbol)`, `(:Community)-[:CONTINUES {overlap}]->(:Community)` | `community_id`, `episode`, `level` — the `CONTINUES` chain gives drift-over-time for free |
| Architecture layer (planned) | `(:Component)`, `(:Layer)-[:ALLOWS_DEPENDENCY/FORBIDS_DEPENDENCY]->`, derived `(:Module)-[:DEPENDS_ON {episode, weight}]->`, `(:Violation)` with `episode`, severity, `CITES` evidence | this is what makes "declared vs actual" views possible |
| Memory / agent context | `(:Memory)-[:PINNED_TO {episode, state}]->(:Symbol)`, `(:Observation)`, `(:MentalModel)-[:GROUNDED_IN]->`, `(:Hint {scope, weight, state})`, `(:Served)`, `(:Outcome)`, `(:HeldOut)` | pins give *knowledge* hotspots — something CodeScene derives from git authors, we get from actual agent usage |
| Cross-repo | `(:Symbol)-[:CROSS_CALL {confidence, module_path}]->`, `(:UnresolvedCross)` | confidence, derivation |
| Derived metrics | computed per episode as enrichment (fan-in/out, churn counts, co-change) — cacheable as `(:Artifact)` with `input_hash` | |

Two facts shape every rendering choice:

1. **The live graph is a label partition** (`(v:Live)`, live rel types) — all dashboard queries run against "current code" by default and against history only where explicitly asked. One traversal, no mega-joins.
2. **Episodes, not commits, are the change unit** for most projections; git commits exist (`:Commit`, `:Assertion` timeline) but the episode DAG is what versions the graph. "Churn" is therefore *episode-granular change* (how many episodes touched a symbol/file) unless we join through to commits.

## 1. Survey

_Survey sections are grouped by lineage (CodeScene line, Thoughtworks line, CodeCity/Leipzig line, …); each ends with a fit assessment against an interactive dashboard on Graphicle's CypherLite backend (data availability, interaction cost, licensing)._

### 1.1 CodeScene — Adam Tornhill (hotspots, change coupling, Code Health, knowledge maps)

**Who did what.** **Adam Tornhill** is the sole technical creator: he built the first version himself in 2015 (company incorporated as **Empear AB**, renamed CodeScene AB; first customer Oct 2015, product GA early 2016; HQ Malmö, Sweden). The product operationalizes his book *Your Code as a Crime Scene* (Pragmatic Bookshelf, 1st ed. 2015, foreword Michael Feathers; **2nd ed. Feb 2024**, foreword Kevlin Henney) and *Software Design X-Rays* (Pragmatic Bookshelf, 2018 — coins "behavioral code analysis"). Business-side: Hamdija Jusufagic (CEO since Dec 2019), Christian Kvist (co-owner) — neither is a technical author. **Attribution corrections (verified):** no "Ludo/Ludvig" exists in any CodeScene source (likely a conflation with **Dr. Markus Borg**, CodeScene's Principal Researcher); **Code Health is Tornhill's concept**, not Tornhill & Bache — its empirical validation is by **Borg et al.** ("Code Red: The Business Impact of Code Quality", Tornhill & Borg, TechDebt 2022, arXiv:2203.04374 — low-quality code: 15× more defects, fixes 124% slower; best-paper award 2024: Borg, Mones, Tornhill, Pruvost). Sources: [codescene.com/company](https://codescene.com/company), [3-year retrospective](https://codescene.com/blog/happy-birthday-3-years/), [Wikipedia](https://en.wikipedia.org/wiki/CodeScene).

**What the visualizations show, and the interaction model:**

- **Hotspot maps** (complexity × churn). Today's CodeScene UI is an interactive **circle/bubble "System Map"**: each large circle = a folder; size tracks activity, color = selected metric (low Code Health → red). Interaction: **drill down system → component → folder → file → function (X-Ray)** by clicking; wheel-zoom; breadcrumbs; details sidebar; **filter by team/authors**; combined-aspect overlays (hotspots + low code health + defects/costs from Jira or commit-message mining). The book-era workflow used rectangle treemaps instead — "treemaps in CodeScene" describes the older, self-built tooling, not the current UI.
- **Change coupling** (the product name for temporal coupling): modules that "change together" — same commit, same author within a time window, or same ticket ID. Visualization: hierarchical graph over the codebase; **hover a file to highlight its co-change dependants; edge color = temporal trend (growing=red, decreasing=blue, stable=yellow)** — you can see whether an intervention is working. File- and component-level; team overlay to expose cross-team logical dependencies; **cross-repo coupling via ticket IDs** rendered as a dependency wheel.
- **X-Ray**: function-level behavioral analysis of hotspots (last ~200 revisions): per-method LOC/complexity/churn, **change coupling between methods** ("two methods change together in 42% of changes"), complexity trends, clone detection prioritized by coupling, and *proximity analysis* (count of intermediate functions between coupled methods) for restructuring recommendations.
- **Code Health (CodeHealth™)**: 1–10 maintainability score combining code-level findings (Brain Methods, DRY violations, God Class) with organizational factors; per file, aggregated per component/system, as **trends over time**; powers PR gates, IDE extensions, and a CodeHealth MCP server.
- **Knowledge maps**: per-author knowledge from deep contribution history (deeper than git blame); knowledge islands, "Knowledge Island in Complex Hotspot" as top risk, knowledge-loss **offboarding simulation** (mark devs as ex- and preview the blast radius); Individual/Team Knowledge Map with **color = primary author/team**.

**Input data → Graphicle mapping.** VCS history + code parsing (+ optional PM tool). Graphicle's equivalents: churn = episodes touching a symbol/file (`*_HIST` windows, `:Episode` DAG; git commits exist as `:Commit`/`:Assertion` for commit-granular coupling); complexity = derived per-episode metrics (fan-out, LOC); knowledge = `(:Memory)-[:PINNED_TO]->(:Symbol)` + observations — *agent* knowledge rather than author knowledge, which is the more honest signal in an agent-heavy codebase.

**Fit: HIGH.** Hotspots, change coupling, and knowledge maps are the highest-value interactive dashboards in this survey, and every input is either present or one enrichment job away. The interaction model to copy: hierarchical drilldown with breadcrumbs, hover-highlight on coupling graphs, trend-colored edges.

**Code Maat** (Tornhill, © 2013, Clojure, GPLv3 — [github.com/adamtornhill/code-maat](https://github.com/adamtornhill/code-maat/)): the CLI companion to the books — mines git/Mercurial/SVN/Perforce/TFS logs into CSV (`summary`, `revisions`, `abs-churn`, `entity-churn`, `entity-ownership`, `coupling` with degree-of-coupling %, `age`, `authors`, …, `-g` layer-grouping file). Complexity must be merged in a separate step. Companion tooling: **maat-scripts** (Python post-processing), **MetricsTreeMap** (Processing sketches; size = complexity, color = churn, keys 1–5 switch layout algorithms), the [Crime Scene gallery](http://www.adamtornhill.com/articles/crimegallery/codecrimescenegallery.htm) (fractal figures, knowledge maps, code age with exponential decay "half-life of two weeks"). Its analyses "have since evolved into CodeScene." **Fit for us: the query templates, not the tool** — each code-maat analysis is exactly one Cypher query against the episode graph (see §4's hotspot and coupling dashboards).

### 1.2 Erik Dörnenburg — the "generated, not drawn" school (Thoughtworks)

**Who did what.** Erik Dörnenburg ([erik.doernenburg.com](https://erik.doernenburg.com/)) — Thoughtworks consultant (Head of Technology, now alumni), co-author of the Thoughtworks Technology Radar; career since the early 1990s (NeXTSTEP). **Verified talk titles** (from his own archive): "Software Visualization and Model Generation" (2006–2009, JAOO/TheServerSide, often co-presented with **Gregor Hohpe**), "Software quality – you know it when you see it" (QCon SF/London 2011, [InfoQ](https://www.infoq.com/presentations/Software-Quality-You-Know-It-When-You-See-It/)), a 2007 [InfoQ interview](https://www.infoq.com/interviews/erik-doernenburg-software-visualization/), and the 2020 Thoughtworks podcast "[Using visualization tools to understand large polyglot code bases](https://www.thoughtworks.com/insights/podcasts/technology-podcasts/code-visualization)" with Korny Sietsma. **Corrections:** no talk titled "Visualizing Software Architecture" or "Four Best Ways to Visualize Software Architecture" by him could be verified — likely conflations.

**What the visualizations show.** A toolbox of *generated* mid-level views (his "1000-ft view" argument, from SpringViz: neither line-by-line reading nor 30,000-ft architecture diagrams fit a large monolith):
- **Metrics tree maps** (2010): nested-rectangle enclosure of the package structure, size/color = metrics — his core "code map".
- **Dependency Structure Matrices** (2010): package×package dependency counts on intersecting axes.
- **Toxicity charts** (2008 "How toxic is your code?"; D3 remake 2013) — deliberately simple per-file bar charts managers can read; co-developed with Chris Brown and Darren Hobbs (name coined by Ross Pettit).
- **Call-graph visualization** (2008): Graphviz Dot from *dynamic* call graphs recorded via AspectJ-woven tests (dynamic chosen because it handles polymorphism).
- **Code city / polymetric views**: exported Java/C# models in Moose MSE format for CodeCity/CodeCrawler — direct collaboration with **Michele Lanza's group at Lugano** (2007–2009).
- "Pain" visualizations (build pain, ESB pain — the latter coins "Resume-Driven Development" as a critique of hand-drawn architecture diagrams).

**Interaction model.** Mostly static generated artifacts (SVG/Dot), later HTML5/D3 with tooltips and filtering; recurring stated interests: **time/trends as a dimension**, and the warning that visualizations tell "half-truths" (color choice biases conclusions). From the 2020 podcast: a **Unix-pipeline style** (analysis tool → text output → glue → D3), Gource for VCS-history animation, co-change analysis, and **Neo4j as a code-model store**.

**Data → Graphicle mapping.** Every input he uses (structure, metrics, dynamic calls, CI, VCS) is a Graphicle facet or one enrichment job away; his "Neo4j as code-model store" recommendation is literally the CypherLite migration decision.

**Fit: HIGH as doctrine, MEDIUM as views.** The 1000-ft, generated, information-dense, honest-color doctrine is the design brief for §4; enclosure/treemap and DSM become two of the recommended dashboards.

### 1.3 Birgitta Böckeler — codebase *understanding*, AI-era (Thoughtworks → Unblocked)

**Who did what.** Birgitta Böckeler ([birgitta.info](https://birgitta.info/)) — Thoughtworks (Germany) consultant developer for many years; **joined Unblocked as of Oct 2026**. Verified work: "Lessons from inheriting another team's codebase" (Thoughtworks blog, 2018), "Onboarding to a 'legacy' codebase with the help of AI" (martinfowler.com *Exploring GenAI*, 2024), "Can AI help rebuild an application without accessing its code?" (2025), "Cultivating Architecture" (Craft 2019, **with Martin Fowler**), "On Pair Programming" (martinfowler.com, 2020, with Nina Siessegger), the Technology Radar entry "Using GenAI to understand legacy codebases", and GOTO talks on AI assistance (2024–2026). **Corrections:** no "Deep Code" sessions at GOTO or elsewhere could be verified (misattribution); "How to Understand a Large Codebase" is not her title; no strangler-fig exploration work by her found; no documented use of CodeScene/Structure101/graph-viz tooling — her verified toolkit is LLM-based exploration (source trees mounted to LLMs via filesystem MCP servers).

**What the method shows.** A four-phase codebase-takeover method (2018): assessment (find entry points — controllers, message handlers; trace call hierarchies; sketch; write READMEs), a handover meeting focused on the departing team's *decision history* rather than code, kickoff workshop, parallel development plus one production deployment for operational familiarity; structure by functional modules, not technical layers. Her 2024+ work is the closest published analog to Graphicle's mission: **LLM-assisted onboarding to messy legacy code** — where AI helps, where it falls short, context engineering for coding agents.

**Interaction model.** Conversational/experimental (structured prompts over an MCP-mounted source tree, workshops, sketches); no interactive visualization tooling.

**Fit: MEDIUM — as a requirements source, not a viz.** She is the voice of the *audience* (agents and humans onboarding to unknown code): her verified workflow — entry points → call hierarchy → sketches → READMEs → decision history — is exactly the navigation a Graphicle dashboard should accelerate. Treat her writings as requirements for §4, not renderings.


### 1.4 Code City — Richard Wettel ("Visualizing Software Systems as Cities")

**Who did what.** Richard Wettel, PhD at the University of Lugano (USI, Faculty of Informatics; thesis "Software Systems as Cities", 2010, in Michele Lanza's group — supervisor affiliation from the thesis hosting at USI/Lugano repositories), with the founding paper "Visualizing Software Systems as Cities" (Wettel & Lanza, VISSOFT 2007) and follow-ups ("CodeCity – 3D Visualization of Large-Scale Software"; ICSE 2009 doctoral consortium "Visual Exploration of Large-Scale Evolving Software"). The CodeCity tool itself is Wettel's: "an integrated environment for software analysis, in which software systems are visualized as interactive, navigable 3D cities", programmed in **VisualWorks Smalltalk on the Moose platform**, OpenGL rendering; it won 1st prize "ated-ICT Ticino" 2008 and was featured in IEEE Software (Jan/Feb 2009). Wettel explicitly borrows the metric-mapping idea from **CodeCrawler's polymetric views** (Michele Lanza's earlier line of work). Sources: [wettel.github.io](https://wettel.github.io/), [VISSOFT 2007 paper](https://wettel.github.io/download/Wettel07b-vissoft.pdf), [thesis PDF (USI)](https://susi.usi.ch/documents/318214/files/2010INFO005.pdf?download=).

**What it shows.** The city metaphor: **classes are buildings, packages are districts**; visible properties of the artifacts encode chosen software metrics. Per the 2007 paper/thesis (secondary-source summary — the metric names could not be re-verified from the PDF text directly): building **height ≈ number of methods** (or another size metric), **base area ≈ number of attributes / LOC**, **color = a third metric or system version/language**. The point is recognizing system shape at a glance: a few skyscrapers = god classes; uniform low-rise districts = well-factored packages.

**Interaction model.** Free navigation in the 3D city (fly/walk camera), selection of buildings, comparing systems/versions side by side; it is an *inspection* environment, not an editing one.

**Data needed → Graphicle mapping.** A containment hierarchy with per-node size/complexity metrics. Graphicle has this natively: `(:Symbol)-[:CONTAINS]->` rollup gives classes/files/modules; per-symbol metrics (LOC, method count, edge count/fan-out) are derivable per episode as enrichment (`architecture-concepts.md` §1 concepts are exactly the mechanism — cache as `(:Artifact)`).

**Fit: HIGH for the metaphor, MEDIUM for 3D.** The city metaphor is the single most legible "whole system at a glance" visualization and maps perfectly onto `CONTAINS` + episode metrics. But Wettel's actual 3D, navigable Smalltalk environment is not reproducible on a web stack cheaply, and 3D conflicts with the pinned-layout requirement (§5) — hence the 2.5D isometric recommendation in §2.3.

#### CodeCity ecosystem: SoftVis3D (SonarQube)

**Who did what.** **Stefan Rinderle** — `stefanrinderle/softvis3d` and the `sonar-softvis3d-plugin` ("Software visualization for your source code", softvis3d.com), LGPL-3.0, three modules (base Java classes, JS frontend, SonarQube plugin). **Note:** the name "Stefan Prasse" sometimes associated with it could not be verified — Rinderle is the author on the repository; a related thesis "Lebendige Software Cities zur Visualisierung von Softwareprojekten" exists at OST (Ostschweizer Fachhochschule) eprints — authorship unverified. Mark uncertain. The repo was **archived Jan 3, 2026** (read-only; 64 stars). Sources: [github.com/stefanrinderle/softvis3d](https://github.com/stefanrinderle/softvis3d/), [softvis3d.com](https://softvis3d.com/), [OST eprints](https://eprints.ost.ch/id/eprint/1056/).

**What it shows / interaction.** A 3D city built from SonarQube metrics (issues, coverage, size), embedded as a SonarQube plugin page — the value-add was zero-setup city visualization in a tool teams already used. Interactions: 3D navigation, metric legend, click-through to SonarQube issues. Historically it offered city + other layout metaphors.

**Fit: MEDIUM.** The lesson to copy is *integration into the tool the team already lives in* (for us: the editor/agent surface, §3), not the tech.

### 1.5 jqa-dashboard — the closest architectural ancestor of this project

**Who did what.** The **softvis-research** group ("Visual Software Analytics", Leipzig University — [home.uni-leipzig.de/svis](https://home.uni-leipzig.de/svis/)); named contributors include David Baum, Pascal Kovacs, Richard Müller and others. `softvis-research/jqa-dashboard`: "A web-based dashboard for software analysis and visualization using software artifacts' data scanned by jQAssistant." Apache-2.0. Sources: [github.com/softvis-research/jqa-dashboard](https://github.com/softvis-research/jqa-dashboard), [showcases](https://home.uni-leipzig.de/svis/showcases.html).

**Correction to the lore (verified from the repo):** jqa-dashboard is **React + D3 + Neo4j bolt driver** — there is **no Three.js** in it. It renders dashboards/2D views over the jQAssistant-scanned graph, not 3D cities. The group's *3D city* tool is a separate project, **Getaviz** ("solve software engineering problems visually by exploring software artifacts in 2D, 3D, and virtual reality", Java, [github.com/softvis-research](https://github.com/softvis-research) — 53 stars, their flagship; the generating engine behind GETAVIZ, VISSOFT 2017). jqa-dashboard's stack is actually jQAssistant + Neo4j + React + D3 + the `@nivo` chart suite + `graph-app-kit` (it started as a Neo4j Desktop graph app — hence the bolt connection and settings page).

**Interaction model.** A configurable dashboard UI over the scanned graph: settings page for the bolt connection, interactive exploration of structure data, Docker images with preloaded sample data (spring-petclinic, jUnit) and an online demo. Research-prototype status: 41 stars, Travis-CI badge (dead service), demo on a raw IP — valuable ideas, unmaintained code.

**Data needed → Graphicle mapping.** This is essentially the same substrate as Graphicle post-migration: labeled property graph from code scanning, queried live for views. Graphicle is a superset (bi-temporality, communities, memory, agent context).

**Fit: HIGH as a blueprint, LOW as a dependency.** Copy the architecture (embedded engine + web dashboard + live Cypher-backed views + preloaded demo data), not the code. The "dashboard as a web app over the scanned graph" is exactly §3's delivery model.

### 1.6 Richard Müller — the researcher behind that stack

**Who did what.** Dr. Richard Müller ([rmllr.de](https://rmllr.de/), [github.com/rmllr](https://github.com/rmllr)) — Leipzig University (Visual Software Analytics group; now industry: developer/architect, product owner, lecturer). PhD thesis 2015, Leipzig: **"Software Visualization in 3D — Implementation, Evaluation, and Applicability"** ([full text](https://ul.qucosa.de/api/qucosa%3A13253/attachment/ATT-0/)). Selected relevant publications (from his [publications list](https://rmllr.de/veroeffentlichungen/)):

- **"Towards an Open Source Stack to Create a Unified Data Source for Software Analysis and Visualization"** (Müller, Mahler, et al., VISSOFT **2018**, [PDF](https://home.uni-leipzig.de/svis/file/repository/vissoft2018_nier_paper.pdf)) — the jQAssistant + Neo4j unified-data-source paper. **Attribution note:** `architecture-concepts.md` §3 cites "software cartography vissoft'18 Nier et al." — "NIER" is almost certainly the **New Ideas and Emerging Results *track*** of VISSOFT 2018, not an author; the paper is Müller et al. (verify the author list from the PDF before quoting).
- "Visualizing Design Erosion: How Big Balls of Mud are Made" (Müller, Baum, Dietrich, Anslow, VISSOFT 2018) — directly relevant to an architecture-drift dashboard.
- "GETAVIZ: Generating Structural, Behavioral, and Evolutionary Views of Software Systems for Empirical Evaluation" (Müller, Baum, et al., VISSOFT 2017) — generative approach: the *views* are generated from the graph, per user/task, rather than one fixed dashboard.
- "Past, Present, and Future of 3D Software Visualization — A Systematic Literature Analysis" (Müller & Zeckzer, 2015); controlled 2D-vs-3D experiments with Kovacs et al. (2014) — the honest evidence base for whether 3D pays off. **This body of work is the empirical argument behind §2.3's "2.5D first, real 3D only if needed" recommendation.**

**What his research shows (in aggregate).** A pipeline from code scanning (jQAssistant) into a graph DB (Neo4j), from which *multiple* visualizations (2D maps, 3D cities, VR) are *generated* — plus the usability studies of which encodings actually work.

**Fit: HIGH as a methodology.** Müller's stack is the blueprint for Graphicle's: scan → labeled graph → generated views. His generative-views idea (GETAVIZ) generalizes to "a dashboard definition is a Cypher query + a rendering spec", which is precisely the design in §4. **Note:** Stephan Diehl's book "Software Visualization" (Springer 2007) is a different author — Müller has no book.

### 1.7 Markus Harrer (feststelltaste) — software analytics, git mining, graphs for legacy code

**Who did what.** Markus Harrer — **Senior Consultant at INNOQ** (Germany), blogging as **feststelltaste** ([feststelltaste.de](https://www.feststelltaste.de/), "Legacy Systems, Software Analytics and the Fundamental Problems of Software Engineering"); microsite [softwareanalytics.de](https://softwareanalytics.de/). **Corrections:** "Software Graph Analysis" is not a site/book of his — the verified artifact is the GitHub repo [`feststelltaste/software-graph-analytics-workshop`](https://github.com/feststelltaste/software-graph-analytics-workshop) (Jupyter + local Neo4j + jQAssistant workshop material); no "Advison"/"adavis" dashboard tool exists anywhere attributable to him; no GOTO talk verified. His current focus (per INNOQ) is **AI-driven / agentic software modernization** ("Getting to Know Your Legacy (System) with AI-Driven Software Archeology", WeAreDevelopers 2025; "Agentic Software Modernization", VibeKode Berlin 2026).

**What the work shows.** Data-driven analyses in Jupyter/pandas over git history, code, and performance data; the flagship graph series is "[Mining performance hotspots with JProfiler, jQAssistant, Neo4j and Pandas](https://www.feststelltaste.de/mining-performance-hotspots-with-jprofiler-jqassistant-neo4j-and-pandas-part-1-the-call-graph/)" — jQAssistant scans Java bytecode into a Neo4j graph, queried from notebooks. He is also adjacent to **CodeCharta** (MaibornWolff's interactive software maps; he maintains a fork and a `jscpd-to-codecharta` converter). Books: co-author of *Software Reviews* (Leanpub 2023, with Koppelt, Wolf, Otten, Starke) and *Quality Tactics*; runs cards42.org and the awesome-software-analytics / awesome-legacy-systems lists.

**Interaction model.** Notebook-driven exploration, not dashboards — his stance is "learn the analysis, then the tool can automate it". The graph→pandas pipeline is the part to steal: query the code graph from a data-analysis environment.

**Fit: HIGH as method.** Graphicle is the productized version of his pipeline (code graph in a graph DB + analytics on top) with the agent loop added; his "AI-driven software archaeology" framing is the closest published narrative to Graphicle's pitch. **Bonus fact:** Harrer is a co-author of the VISSOFT 2018 NIER paper below.

**The "Nier et al." mystery — resolved.** `architecture-concepts.md` §3 cites "software cartography vissoft'18 Nier et al.". Two different things were conflated:
1. The VISSOFT **2018 NIER-track** paper "*Towards an Open Source Stack to Create a Unified Data Source for Software Analysis and Visualization*" is by **Richard Müller, Dirk Mahler, Michael Hunger, Jens Nerche, Markus Harrer** (DOI 10.1109/vissoft.2018.00019) — "Nier" is a double misread of the **NIER** track and the author **Nerche** (and the user of this repo is a co-author).
2. The actual **software cartography** paper is "*Software Cartography: thematic software visualization with consistent layout*" by **Adrian Kuhn, David Erni, Peter Loretan, Oscar Nierstrasz** (Software Composition Group, University of Bern; WCRE 2008 short, JSME 2010, DOI 10.1002/smr.414) — that is where the *consistent/stable layout* argument comes from (§1.10, §5). Suggest fixing the citation in `architecture-concepts.md` §3 when convenient.

### 1.8 Carola Lilienthal — architecture analysis at scale (WPS)

**Who did what.** Dr. Carola Lilienthal — **Managing Director of WPS Workplace Solutions GmbH (Hamburg)**, head of its software-architecture practice; architecture reviews since 2003 (with Johannes Rost since 2004). PhD 2008, University of Hamburg (supervisors Christiane Floyd and Claus Lewerentz), on "Komplexität von Softwarearchitekturen – Stile und Strategien". Books: **"Langlebige Software-Architekturen"** (dpunkt 2015, 4th ed. 2024; English: *Sustainable Software Architecture*, 2019), "Software Architecture Metrics" (O'Reilly 2022), "Domain-Driven Transformation" (2023, with Henning Schwentner); iSAQB advisory board, curator of its DDD module. Her team also maintains Egon.io (Domain Storytelling). **Corrections:** "informatikBüro" and any former-Zühlke affiliation could **not** be verified — every public bio places her at WPS; **"Schatzkarten"/treasure maps is not her term** (unverified) — her verified artifacts are the MMI, actual-vs-target gap visualizations, and domain-vs-technical layer comparisons; the scale is **100+ analyzed systems (2015 book) → 300+ (current site) → 400+ MMI-analyzed systems (2025 interviews)**, not "30+".

**What the method shows.** A 1–4 day, tool-supported architecture review with the dev team: technical-debt analysis, **visualization of the gap between implemented actual architecture and planned target architecture**, a prioritized refactoring roadmap, optional formal expert reports. The signature metric is the **Modularity Maturity Index (MMI)** — 0–10, weighted: modularity 45% (domain/technical modularization 25%, internal interfaces 10%, proportions 10%), hierarchy 30% (layering 15%, cycles 15%), pattern consistency 25%; bands: 8–10 low debt, 4–8 debt but refactorable, <4 refactor-or-replace. The rationale is cognitive: developers spend ~70% of their time *understanding* code. Visual repertoire (talks/books, 200+ color figures from real systems): **the same dependency view colored by domain module vs. by technical layer**, cycle views, "legacy black holes", technical-debt erosion over time, and Team Topologies/cognitive-load alignment.

**Interaction model.** Consultant-driven; deliverables are reports and figures rather than interactive tools. The tool ecosystem she names in talks: **Sotograph, Structure101, Lattix, jQAssistant, Teamscale, Seerene, aim42** ([software-architektur.tv ep. 57](https://software-architektur.tv/2021/04/30/carola-lilienthal-zu-langlebigen-software-architekturen.html)).

**Data → Graphicle mapping.** The MMI is computable from the enriched graph: modularity = Leiden communities vs. module structure (modularization), internal interfaces, size proportions; hierarchy = declared `(:Layer)` conformance and `(:Module)-[:DEPENDS_ON*2..6]->` cycles; pattern consistency needs the concept rules. I.e. MMI-as-enrichment is a direct consumer of the `architecture-concepts.md` layer.

**Fit: HIGH.** "Actual vs. declared" and the MMI are exactly the enriched-graph plan, and her domain-vs-technical coloring is the cheapest high-value overlay on the community map (§4 D1).

### 1.9 Neo4j NVL — status (web-verified; local usage in §2.1)

**What it is.** NVL, "a collection of libraries that can be used to build custom graph visualizations like those used in Neo4j Bloom" — TypeScript, npm `@neo4j-nvl/*`. **Repo is [github.com/neo4j/neo4j-nvl](https://github.com/neo4j/neo4j-nvl)** (the `neo4j/nvl` URL 404s). **Current version 2.0.0** (Sept 2026): renderer **and** physics engine upgraded to **WebGL2**; layouts `forceDirected`, hierarchical (dagre), circular, plus **`clusterBy` clustering with a cluster interaction API**; the old Cytoscape-based layouts were deprecated in 1.2.0 and removed in 2.0.0. Lineage: 0.3.5 (Feb 2025) made canvas the default renderer; **1.0.0 (Sept 2025) was the "first major release"** — no page found still labeling it early access, but no verbatim "GA" statement either (uncertainty). First-class TypeScript API ([neo4j.com/docs/api/nvl/current/](https://neo4j.com/docs/api/nvl/current/)); packages `base`, `interaction-handlers`, `react`. Data: plain arrays of nodes/rels (`new NVL(container, nodes, rels, options, callbacks)`). Interactions via the handlers package: click/double-click/right-click, hover, drag-node, pan, zoom, box-select, lasso; core callbacks `onLayoutDone`, `onInitialization`, `onZoomAndPan` (2.0). SVG export for static images (1.1+). **No layout-persistence API exists** (verified absent from docs/changelog) — another reason pinned layouts must live in *our* graph, not in the renderer (§5). **License: NOASSERTION on GitHub / "SEE LICENSE IN LICENSE.txt" — effectively proprietary** (the Neo4j Visualization Library License, per cypherlite's ADR-0151, limits use to Neo4j products). Boilerplates: [neo4j-devtools/nvl-boilerplates](https://github.com/neo4j-devtools/nvl-boilerplates) (Angular/React/plain-JS).

**Fit: HIGH technically, licensing gate unresolved.** CypherLite pins 1.2.x locally (canvas renderer); 2.0.0's WebGL2 + `clusterBy` are attractive but the pin keeps the known-good recipe. Decide the license question before making NVL the default for D4/D8.

### 1.10 DSMs, Sonargraph/Structure101-style views, cartography research, CodeCharta, agent-era work

**Dependency Structure Matrices.** Coined by **Donald V. Steward** ("The Design Structure System", IEEE Trans. Engineering Management, 1981) in systems engineering; brought to broad software attention by **Neeraj Sangal & Ev Jordan (Lattix) with Vineet Sinha & Daniel Jackson (MIT)**, "Using Dependency Models to Manage Complex Software Architecture", OOPSLA 2005. A square matrix (same elements on rows and columns; a mark/weight in cell (r,c) = a directed dependency); **hierarchical and collapsible** (module → package → class); triangularization = good layering (feedback marks near the diagonal = cycles). Tool interactions worth copying (verified from IntelliJ's DSM view, which is the best-documented): click a cell to see the actual dependencies, "explore dependencies between", green = dependents / yellow = dependencies / **red = cyclic**, cell shading by dependency count, usage-sorted rows so well-structured projects form a lower-left triangle. Tools: **Lattix** (Sangal; status dormant-looking, uncertain), **NDepend** (Patrick Smacchia, .NET), **Sonargraph** (hello2morrow — Alexander von Zitzewitz, Dietmar Menges), **Structure101** (Headway Software, Chris Chedgey; **acquired by SonarSource Oct 15, 2024** and folded into SonarQube). **CodeMaat hotspots** are covered in §1.1.

**Software cartography (the real paper).** "Software Cartography: thematic software visualization with consistent layout" — **Adrian Kuhn, David Erni, Peter Loretan, Oscar Nierstrasz**, SCG University of Bern (WCRE 2008; JSME 2010). Artifacts are placed so **position reflects vocabulary similarity** (LSI → multidimensional scaling → 2D), and the *consistent layout* lets multiple **thematic maps** — coupling, metrics, evolution — be rendered as atlas pages over the same stable geography, comparable across views and across versions. This is the theoretical basis for §5's pinning requirement; Graphicle's communities + `CONTINUES` chains are a better geography than LSI similarity, and the thematic-atlas model is exactly D1's overlay selector. (Related: TUM/sebis "Softwarekartographie" for *application landscapes* in enterprise architecture; CodeSurveyor, VISSOFT 2015.) **"Philipp Euerer" — not found** in any source; treat that recollection as unverifiable.

**CodeCharta** (MaibornWolff, open source) — "transforms complex software architecture and code metrics into interactive, customizable visual maps": parsers produce a `cc.json` (file tree + metrics), the web app renders interactive map/city visualizations with metric overlays; the metaphor family is covered in "Advanced Visual Metaphors and Techniques for Software Maps" (VINCI 2019). **The closest existing open-source product to D1/D2** — worth a look for interaction ideas and the cc.json separation of *analysis* from *visualization* (same separation as our query-pack/render-spec split). Harrer is adjacent to it (§1.7).

**Agent-era work (brief).**
- **Aider's repo map** — Paul Gauthier ([aider.chat/docs/repomap.html](https://aider.chat/docs/repomap.html)): tree-sitter symbols + a **graph-ranking algorithm over the file-dependency graph** (PageRank-style), emitted as a *textual* tree within a token budget per request. The direct precedent for "serve the code graph to the model" — Graphicle's `ask`/hints are the queryable-graph generalization; a repo-map *view* (ranked tree, same ranking, rendered for humans) is a trivial extra dashboard.
- **DeepWiki** — Cognition (the Devin company), May 2025: LLM-generated wiki + diagrams for public GitHub repos, engine open-sourced. **CodeWiki** — FPT Software AI4Code (arXiv 2510.24428, Oct 2025): repository-level generated wikis with interactive exploration, explicitly targeted at humans *and coding agents*. Both are documentation-first rather than graph-first; Graphicle's differentiator is that its maps are *grounded in a versioned, queryable graph*, not one-shot generations.
- **Repo visualizers** — githubocto/repo-visualizer (SVG bubble map from a GitHub Action) and similar; long tail of 2025–26 LLM-code-graph tools (GitNexus, GraphRepo, CodeMap…) with no verified adoption. GitHub's dependency graph and Sourcegraph remain navigation/index products, not graph *visualization* (Sourcegraph viz unverified).
- **The pattern across all of it** (worth stating in the pitch): every approach in this survey is a *projection of the same code graph* into a different space — matrix (DSM), geography (cartography), city (CodeCity), bubble map (CodeScene), token tree (Aider). Graphicle owns the substrate; dashboards are just projection functions over it.

## 2. Rendering technology options

### 2.1 Neo4j NVL — the known quantity

NVL (`@neo4j-nvl/base` + `interaction-handlers` + `layout-workers`, currently 1.2.x) is already proven in this exact ecosystem — cypherlite's own surfaces use it:

- **`cypherlite/examples/wasm-playground`** (React + Vite): uses `@neo4j-ndl/react-graph`'s `GraphVisualization` (a Needle wrapper around NVL) plus the raw `@neo4j-nvl/*` 1.2 packages; extracts a graph view from a query result with a **node limit of 1000**, `fit()` on container resize, custom touch handling (`useNvlTouch`) because NVL's interaction handlers are mouse-only.
- **cypherlite ADR-0151** (`docs/adr/0151-cookbook-graph-view-nvl.md`, "Cookbook graph view: interactive NVL with double-click expand"): the framework-free recipe — `@neo4j-nvl/base` + `interaction-handlers` on the **canvas renderer** with the `d3Force` layout, `disableWebWorkers: true` and `disableTelemetry: true`. Measured bundle: **~1.8 MB / ~520 KB gzip** (mostly NVL + cytoscape). Comfort zone: **~1000 nodes on canvas**; the WebGL renderer scales past 100k nodes but drops caption fidelity. The ADR's interaction model is exactly what a Graphicle explorer needs: wheel/pinch zoom, drag node (layout settles around it), hover/click inspector for properties, buttons (zoom/fit/reset), and **double-click expand running two read queries against the embedded engine** (`MATCH (n)-[r]-(m) WHERE id(n) = $id RETURN r, m LIMIT 100` + connect-the-neighbours).
- **Licensing caveat (flagged, not resolved)**: NVL's license is effectively proprietary — GitHub shows NOASSERTION ("SEE LICENSE IN LICENSE.txt"), and ADR-0151 characterizes the Neo4j Visualization Library License as limiting use "to use with Neo4j products". CypherLite is a neo4j-labs project and Graphicle embeds it, but whether a product built on CypherLite qualifies needs an explicit check before NVL becomes the default renderer. (Current NVL status, verified from the web, is in §1.9: repo `neo4j/neo4j-nvl`, v2.0.0 with WebGL2 renderer/physics and a `clusterBy` clustering API, no layout-persistence API — cypherlite pins 1.2.x/canvas, which is the battle-tested recipe here.)

**Where NVL wins**: any *graph-shaped* dashboard where the point is exploring relationships (call-graph explorer, community clusters, cross-repo links), because pan/zoom/drag/inspect/expand come for free and it behaves like the Neo4j tools people know.

**Where NVL loses**: pinned/stable layouts (its layouts are force-based; positions *can* be supplied, but layout is not its strength), and anything non-graph-shaped — treemaps, heatmaps, DSMs, timelines, 3D cities. Those are custom SVG/D3 or Three.js territory.

### 2.2 Custom SVG/D3 — the cartography engine

For the map/city metaphors (architecture-concepts §3's cartography direction) the shapes are enclosures (treemaps), districts, elevations, heatmaps, matrices, timelines — all 2D, all fully controllable, all deterministic. `d3-hierarchy` (treemap/enclosure), `d3-force` (with *fixed* positions for the pinning requirement), `d3-scale` + SVG give exact control over the "same map every episode, diff the differences" requirement. Cost: hand-built zoom/pan/tooltips (a few hundred lines, or `d3-zoom`/`panzoom`), no free inspector. This is what cypherlite's cookbook diagrams already do for static renderings (`d3-force` → SVG), so there is local precedent and code to crib from.

### 2.3 3D city (Three.js) — the flashy option

The CodeCity/jqa-dashboard lineage (survey §1). Strong demo appeal, real depth-cue value for size×height×color encodings, but: heavy dependency, camera navigation is an acquired skill, and — decisive — **3D cities resist the pinned-layout requirement** (a fixed camera path + fixed building grid does work, that's the point of jqa-dashboard's stable city; still, screenshot-diffing a 3D scene is harder than diffing an SVG). Recommend a **2.5D isometric "city"** first: treemap districts (pinned) with pseudo-3D extrusion on top of the 2D layout — CodeCity semantics, SVG cost, diffable pixels. Promote to real 3D only if the isometric version proves too limited.

### 2.4 Rendering verdict (preview — detailed per-dashboard in §4)

- Graph-exploration dashboards → **NVL** (pending the license check; fallback: `d3-force` SVG, the ADR-0151 alternative already evaluated).
- Map/city/matrix/timeline dashboards → **custom SVG/D3**, layout coordinates **pinned in the graph** (§5).
- Nothing needs WebGL/3D to start.

## 3. Delivery: local webpage + VS Code

**Same bundle, two hosts.** Build the dashboard series as one static web app (a single JS bundle + assets; the wasm-playground and ADR-0151 show both React and framework-free patterns work with NVL and D3). Ship it inside the Graphicle binary (embedded assets, like the existing single-binary philosophy) and serve it from the existing axum HTTP server:

- **Local webpage**: `graphicle serve` already runs HTTP; add `GET /dashboards` (static app) and `GET /dashboards/api/:name` JSON endpoints backed by read-only Cypher queries (one endpoint per dashboard, parameterized: repo, episode, focus symbol). Auth via the existing credential machinery; CORS not needed (same origin). Local-first, no telemetry — mirror ADR-0151's `disableTelemetry` stance in the app config.
- **VS Code webview**: a small extension (TypeScript; `sdk/typescript` `@graphicle/sdk` already exists as the client) with a `Graphicle: Dashboard` command that opens a webview panel. Two hosting modes, in order of preference:
  1. **Iframe to the local server** (`http://127.0.0.1:<port>/dashboards?…`): zero bundle duplication, the webview needs only a CSP allowing `127.0.0.1`. Con: requires the server to be running.
  2. **Bundled assets + `postMessage` API**: the extension hosts the same static files and proxies queries over the SDK; works when no server is listening (VS Code extension host runs Node; the Python embedding path of cypherlite is not needed).
  Selection/focus state should round-trip: clicking a hotspot in a dashboard can send a message (webview → extension → `vscode.window.showTextDocument` at `relpath:line`) so a dashboard is a navigation surface into the editor, not a dead-end picture. Evidence links (`relpath:line`, cited excerpts) make this natural — every dashboard row should be clickable.
- The macOS desktop app (`desktop/`, Swift) can add a `WKWebView` pointing at the same server URL later; do not design for it now.

## 4. Dashboard series

Common shape (jqa-dashboard's architecture + GETAVIZ's generative idea + Dörnenburg's doctrine): **a dashboard = a named, versioned Cypher query (or query pack) + a rendering spec + a parameter set** (`repository_id`, `episode`, focus, overlay). The web app fetches `GET /dashboards/api/{name}?repo=…&episode=…` and renders; the same JSON feeds the VS Code webview. All queries below run against the live label partition and are cheap (2–3-hop traversals, p50 well under a millisecond per the migration's load tests at 100k nodes / 500k rels); anything heavier (churn counts, coupling) is precomputed as an enrichment job per episode and cached as a graph artifact, not recomputed per page load.

Ordered by build priority (§6 explains the sequencing).

### D1. System map — the cartography flagship

**Shows.** The whole repo as a map: **districts = communities** (Leiden, current episode), blocks/bubbles inside districts = symbols or files, district borders optionally = declared `:Component`/module boundaries. Overlay selector (the Lilienthal trick — same map, different coloring): domain concept (`:DomainConcept`), layer, churn, code-health proxy, test-vs-prod, memory coverage. Diverging overlays from declared structure are the story: a community leaking across a component border is a finding, not a rendering artifact.

**Query sketch.**

```cypher
// districts + members + per-symbol metrics (one round trip, episode-parameterized)
MATCH (c:Community {episode: $ep})-[:HAS_MEMBER]->(s:Symbol)-[:HAS_VERSION]->(v:Live)
OPTIONAL MATCH (s)-[:DOMAIN]->(d:DomainConcept)
RETURN c.community_id AS district,
       collect({id: s.stable_id, kind: s.kind, qname: s.qualified_name,
                relpath: s.relpath, domain: d.name, loc: v.loc,
                fanout: v.fanout, churn: v.churn}) AS members
```

(with `loc`/`fanout`/`churn` per-episode enrichment properties on `:SymbolVersion` — see D2's job; overlay variants are `OPTIONAL MATCH` swaps, i.e. one query pack per overlay).

**Rendering.** Custom **SVG/D3** — districts as pinned hulls/enclosures (d3-hierarchy), members as bubbles. **Pinned layout** per §5: coordinates stored on `(:Community)`/(:Component) layout nodes, so the map is the same map every episode. NOT NVL (force layout would break the pinning contract); NOT 3D.

**Interactions.** Click district → member list with `relpath:line` evidence links (opens in editor via webview message); overlay dropdown; episode slider (see D6); hover inspector; search box (fulltext/vector `SEARCH` on symbols jumps/zooms to the member).

**Data dependencies.** Communities (exist); per-symbol enrichment metrics (new job); declared architecture (architecture-concepts Phase 1) for the border overlay — D1 v1 can ship without it.

### D2. Hotspot city

**Shows.** CodeScene semantics in a CodeCity skin: **buildings = files (or big symbols), height = size/complexity (LOC or fan-out), color = churn** (episodes touching the file), districts = modules/communities. Hotspots pop as tall red towers. 2.5D isometric, not real 3D (§2.3) — treemap grid is pinned, so towers stay put across episodes.

**Query sketch.** Churn per file, episode-counted (precompute as job, store on the version node):

```cypher
// enrichment job: per-episode churn = number of episodes whose projection touched the file
MATCH (f:File)-[:HEAD]->(:Object) WHERE f.repository_id = $repo
MATCH (s:Symbol {relpath: f.path})-[h:CONTAINS_HIST|HAS_VERSION*0..1]->()  // projection-touch proxy
...
// read path for the dashboard:
MATCH (f:File {repository_id: $repo})-[:HEAD]->(o:Object)
RETURN f.path AS file, o.bytes AS size, f.churn_episodes AS churn,
       f.churn_trend AS trend   // ratio: last-third-of-episodes churn vs overall
```

(The exact churn definition — symbol-version flips vs. object sha changes vs. episode labels — is an open question; `Object` sha changes at `:HEAD` are the cheapest exact signal of file change per episode. Commit-granular churn via `:Commit`/`:Assertion` is the upgrade path.)

**Rendering.** Custom SVG/D3 isometric extrusion over a pinned d3-hierarchy treemap. Three.js later if the isometric version feels limiting.

**Interactions.** Click tower → file detail (churn history sparkline, memories pinned to its symbols, X-Ray-style per-symbol breakdown); sort/filter (top-N hotspots list beside the city — Tornhill's "1.2% of the code = 12.5% of the effort" framing works as a table, not just a picture); episode slider.

**Data dependencies.** Churn enrichment job (new); everything else exists.

### D3. Change-coupling matrix ("temporal coupling")

**Shows.** CodeScene's change coupling as a **matrix heatmap** (modules × modules, or files × files for a focus district): cells = co-change frequency within episodes (or within commits, once `:Commit` granularity is wired); cell color split by trend (grew recently = red, decaying = blue — CodeScene's edge-trend idea transplanted to cells). This is the DSM (Dörnenburg 2010) applied to *evolution* rather than static imports — deliberately both, side by side: co-change matrix next to static-dependency matrix; cells that are hot in one and cold in the other are the interesting ones (hidden coupling / dead dependencies).

**Query sketch.** Precompute per episode-pair co-change (job), or one query for a focus scope:

```cypher
// co-change within episodes, module-level (assumes :DEPENDS_ON-style Module rollup or CONTAINS lift)
UNWIND $episodes AS ep
MATCH (e:Episode {episode: ep})-[:HAS_MEMBER|SCOPED*0..1]->()  // episode → changed files/symbols
// simpler: for each episode, the set of relpaths whose symbols gained/closed a version:
MATCH (s1:Symbol)-[h1:CALLS_HIST|CONTAINS_HIST]->() WHERE h1.valid_from_episode = ep
...
// read path: pair frequencies
MATCH (f1:File {repository_id:$repo}), (f2:File {repository_id:$repo})
RETURN f1.path, f2.path, count(e) AS cochanges
```

(Concrete shape depends on the churn job's choice of "changed in episode" — same dependency as D2; sketch is indicative, the job will materialize `(:Coupling {degree, trend})-[:BETWEEN]->(f1),(f2)`.)

**Rendering.** Custom SVG heatmap (d3-scale diverging palette), module order pinned by canonical sort; hover cell → both rows/cols highlighted + the co-changing files listed; click → the commits/episodes that co-changed them, with evidence links.

**Data dependencies.** Churn/co-change job; module rollup (`DEPENDS_ON` derivation from architecture-concepts §6 — or a lighter `CONTAINS` lift).

### D4. Call-graph explorer / impact blast radius

**Shows.** One focus symbol's live neighborhood: callers/callees transitively, `IMPORTS`, `INHERITS`, cross-repo `CROSS_CALL`; the agent-facing "blast radius" (`arch_impact` made visual).

**Query sketch.** Exactly the ADR-0151 pattern, ported to Graphicle's schema:

```cypher
// initial view (depth-1, live partition)
MATCH (s:Symbol {stable_id: $id})-[r:CALLS]->(t:Symbol)
RETURN s, r, t LIMIT 100
// expand on double-click
MATCH (n)-[r:CALLS|IMPORTS|INHERITS|CROSS_CALL]-(m:Symbol) WHERE id(n) = $id RETURN r, m LIMIT 100
// blast radius (bounded transitive):
MATCH (s:Symbol {stable_id:$id})-[:CALLS*1..3]->(t:Symbol) RETURN s, t, count(*) AS paths
```

**Rendering.** **NVL** (canvas renderer, `d3Force`, `disableWebWorkers`/`disableTelemetry`, 1000-node comfort cap — the exact recipe from cypherlite's ADR-0151 and wasm-playground). Pending the NVL license check (§2.1); fallback is a `d3-force` SVG with hand-rolled expand — ADR-0151 already priced that alternative.

**Interactions.** The full NVL set: pan/zoom/drag, hover/click inspector (properties, memories pinned to the symbol), double-click expand, fit/reset buttons; "who calls this?" vs "what does this call?" direction toggle; depth slider (1–3); community color preset (nodes colored by their `:Community` — ties D4 back to D1).

**Data dependencies.** None new — this works on the migration output alone. **Cheapest dashboard; natural first build.**

### D5. Architecture conformance — declared vs. actual (DSM + violations)

**Shows.** The declared architecture (`:Layer`/`:ALLOWS_DEPENDENCY`, `:Component`, `:DomainConcept` from the concept rules) against the lifted actual dependencies (`:DEPENDS_ON {weight}` between modules): a **DSM/heatmap** where cells hold edge weight, **violating cells marked** with severity, plus a violation list with `:Violation`-`:CITES` evidence. Lilienthal's gap view + jQA's constraint report, interactive.

**Query sketch.**

```cypher
MATCH (a:Module)-[d:DEPENDS_ON {episode:$ep}]->(b:Module)
OPTIONAL MATCH (la:Layer)<-[:PART_OF_LAYER]-(ca:Component)<-[:REALIZED_AS]-(a)
OPTIONAL MATCH (lb:Layer)<-[:PART_OF_LAYER]-(cb:Component)<-[:REALIZED_AS]-(b)
RETURN a.name AS from, b.name AS to, d.weight AS weight,
       la.name AS fromLayer, lb.name AS toLayer,
       EXISTS { MATCH (la)-[:FORBIDS_DEPENDENCY]->(lb) } AS forbidden
ORDER BY a.name, b.name
// violation rows with evidence:
MATCH (v:Violation {episode:$ep})-[:CITES]->(sym:Symbol)
RETURN v.rule, v.severity, sym.relpath, sym.qualified_name
```

**Rendering.** Custom SVG DSM (pinned order); violations side panel; click cell → the concrete `CALLS`/`IMPORTS` edges behind the aggregate (drill from module matrix to symbol list — one more query).

**Interactions.** Toggle declared/actual/constraints; ratchet view ("7 cycles today, was 9"); click-through to evidence.

**Data dependencies.** architecture-concepts Phases 1–2 (concept rules, `DEPENDS_ON` lift, violations). Builds after that layer lands.

### D6. Architecture drift timeline

**Shows.** Time as the first-class axis (the bi-temporal superpower — nothing in the survey's tools has this): per-episode community evolution via `(:Community)-[:CONTINUES {overlap}]->`, violation counts by severity, MMI-style aggregate score, coupling totals, cross-repo edge counts. Müller/Baum's "Visualizing Design Erosion" and "how big balls of mud are made", made live.

**Query sketch.**

```cypher
MATCH (e:Episode) WHERE e.repository_id = $repo
WITH e ORDER BY e.sequence
OPTIONAL MATCH (e2:Episode {episode: e.episode})<-[:OF]-(:Violation) 
// community continuity:
MATCH (c:Community {episode: e.episode})-[k:CONTINUES]->(c2:Community)
RETURN e.sequence AS seq, e.episode,
       count(DISTINCT c) AS communities,
       avg(k.overlap) AS continuity,
       count { (:Violation {episode: e.episode}) } AS violations
ORDER BY seq
```

(indicative — the real query aggregates per-episode markers; all are label/property scans over `:Episode` + per-episode counts, precomputable into a per-episode rollup node if needed).

**Rendering.** Custom SVG/D3 line/area + swimlanes (community identity lanes colored consistently via `CONTINUES` chains). Episode slider shared with D1/D2 — scrubbing the slider re-renders *any* dashboard at episode N; that cross-dashboard scrub is the series' signature interaction.

**Data dependencies.** Episodes, `CONTINUES` chains (exist); violations (Phase 2). Partial value without the architecture layer (community continuity alone is worth shipping).

### D7. Knowledge & memory map (the agent-era original)

**Shows.** What no tool in the survey has: **agent knowledge as a first-class overlay**. Treemap of files/symbols colored by memory coverage — memories pinned (`PINNED_TO`), observations, mental models grounded, hints scoped — with **staleness** highlighted (pins whose symbol moved or whose version window is closed). Knowledge islands (CodeScene's risk framing, but for agent context): a hotspot with zero memories/observations is where the next agent session starts blind.

**Query sketch.**

```cypher
MATCH (f:File {repository_id:$repo})-[:HEAD]->(:Object)
OPTIONAL MATCH (s:Symbol {relpath: f.path})<-[p:PINNED_TO]-(m:Memory)
RETURN f.path AS file,
       count(DISTINCT m) AS memories,
       count { (s)<-[o:PINNED_TO]-(:Observation) } AS observations,
       sum(CASE WHEN p.state <> 'live' OR p.episode < $ep THEN 1 ELSE 0 END) AS stale_pins,
       f.churn_episodes AS churn
```

(indicative; `(:Hint {scope, target, weight})` and `:Served`/`:Outcome` counts are the same shape.)

**Rendering.** Custom SVG treemap (d3-hierarchy, pinned); churn as a secondary overlay — the 2×2 of churn × knowledge is the actionable quadrant chart.

**Interactions.** Click file → its memories/observations with cited excerpts; filter by memory kind/state; "blind spots" preset (high churn, zero knowledge) — that list is also directly publishable as an MCP hint pack, closing the loop back into agent context.

**Data dependencies.** Memory machinery (exists). No new jobs.

### D8. Cross-repo & external dependency view

**Shows.** `CROSS_CALL`/`:UnresolvedCross` between repositories/modules: a bipartite-style map of who calls out, with confidence and `module_path` targets; unresolved externals as a separate "fog bank" district. Also usable for dependency-graph risk views (the cypherlite cookbook's blast-radius recipe transplanted).

**Query sketch.**

```cypher
MATCH (s:Symbol {repository_id:$repo})-[c:CROSS_CALL]->(t)
RETURN s.relpath, s.qualified_name, t.target_repository_id, t.symbol, c.confidence, c.module_path
UNION
MATCH (u:UnresolvedCross) WHERE u.repository_id = $repo
RETURN u.relpath AS s_relpath, u.symbol AS s_qualified_name,
       null AS target_repository_id, u.target AS symbol, u.confidence, u.module_path
```

**Rendering.** NVL (bipartite/force) or a Sankey-style custom SVG (flow weight = edge count); either is small data.

**Interactions.** Filter by confidence/derivation; click → source evidence; "explain this link" → the memory/observation that mentions it, if any.

**Data dependencies.** None new.

**Deliberately not in v1:** a 3D city (§2.3), a VR view (Getaviz territory), PR-level code health (needs per-function complexity extraction beyond fan-out), author-knowledge maps (CodeScene's people analytics — Graphicle is agent-centric; authorship would need git identity joins).

## 5. The pinned/stable-layout requirement

`architecture-concepts.md` §3 already commits to it: *"the vissoft paper's 'consistent, stable map layout' point argues for pinning layout coordinates on `(:Component)` nodes so diffs between episodes are visible, not re-laid-out."* (The paper behind that argument is Kuhn, Erni, Loretan & Nierstrasz's *Software Cartography* — §1.10; their "thematic maps over a consistent geography" is precisely the model below.) Concretely:

- **Layout is data, not rendering.** Compute a layout once (per project), store coordinates as properties on the nodes being mapped — e.g. `SET c.map_x, c.map_y, c.map_level` on `(:Component)`/`(:Community)`, or a dedicated `(:LayoutNode {layout:'map-v1', x, y, r})-[:POSITIONS]->(n)` so multiple layouts (map v1/v2, city) coexist and relayout is a new layout id, never a mutation of the old one.
- **Incremental placement**: new nodes are placed near their community's centroid / parent's enclosure by a cheap local step (no global re-layout); nodes that vanish leave a *gap* (tombstone marker for one episode) rather than collapsing the district — that's how drift becomes legible.
- **Relayout is an explicit, versioned event** ("the map got too crowded"), never a side effect of querying. The `(:Community)-[:CONTINUES {overlap}]->` chain keeps identity stable across episodes so colors/positions track concepts, not ids.
- **Diffing**: with pinned coordinates, an episode-over-episode diff is two SVG renders with the same geometry and changed fills — trivially expressible as an animation or a side-by-side, and directly consumable by agents as images if desired.
- Same rule for the isometric city (pinned treemap grid) and the DSM (module order pinned by a canonical sort, only cell values change).

## 6. Phased recommendation

**Phase 0 — prerequisite (already underway).** The CypherLite migration itself. D1–D8 all read the post-migration schema; nothing here should start before the store swap lands.

**Phase 1 — the spine + the cheapest dashboard (D4).** Build the delivery spine once: embedded static app served at `/dashboards` by the existing axum server, `/dashboards/api/{name}` JSON endpoints (read-only connections from the pool), the VS Code extension with an iframe-first webview (§3), and the click-to-editor message path. On top of it, ship **D4 (call-graph explorer)**: it needs *zero* new data or jobs, exercises the whole stack end to end, and forces the one blocking decision early — **run the NVL license check now**; if it fails, build D4 on `d3-force` SVG (the ADR-0151 alternative) and keep NVL out of the plan. Sequencing rationale: infra + easiest high-value view first, license risk surfaced before any NVL investment.

**Phase 2 — the cartography core (layout pinning, D1, D6, D2).** The pinned-layout machinery of §5 is its own small project (layout-as-data, incremental placement, tombstones, relayout-as-versioned-event) — build it once, before any map-like dashboard. Then **D1 (system map)** with two overlays (community color, churn) and **D6 (drift timeline)** with the shared episode slider, both on communities that already exist. The **churn/co-change enrichment job** lands here (it is D1/D2/D3's shared dependency; define "changed in episode" via `Object` sha flips, with commit-granularity as the upgrade). **D2 (hotspot city)** follows on the same pinned treemap grid — isometric SVG only; defer any Three.js decision until D2 is in daily use.

**Phase 3 — analytics on existing data (D7, D3, D8).** **D7 (knowledge & memory map)** needs no new jobs and is the most Graphicle-original view — build it early in the phase. **D3 (coupling matrix)** consumes the Phase-2 co-change job (plus the module rollup). **D8 (cross-repo)** is small and independent; slot it wherever there is slack.

**Phase 4 — the architecture-dependent views (D5 + MMI).** **D5 (conformance DSM + violations)** is gated on `architecture-concepts.md` Phases 1–2 (concept rules, `DEPENDS_ON` lift, `:Violation`); plan it with that work rather than after it. Lilienthal's MMI as a per-episode aggregate score rides the same enrichment and becomes D6's headline metric.

**What NOT to build:** real 3D/VR (until the isometric city demonstrably hurts), author-people-analytics (agent knowledge is the honest signal here), PR-level code-health scoring (a fair Code Health clone needs per-function complexity extraction — a project of its own), and any LLM-generated-wiki view (DeepWiki/CodeWiki territory; Graphicle's maps are grounded in a versioned graph, which is the differentiator — don't blur it).

**Two decisions to make now:** (1) NVL license — gates the renderer for D4/D8; (2) churn definition at episode granularity — gates D2/D3 and the hotspot story's honesty. Everything else can be decided per-phase.
